//! Read-only locators shared by the Claude and Codex usage sources.
use super::super::common::{UsageRuntime, UsageScope};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "store", rename_all = "lowercase", deny_unknown_fields)]
pub(in crate::usage) enum Route {
    Claude {
        path: String,
    },
    Codex {
        path: String,
    },
    Opencode {
        path: String,
    },
    Pi {
        path: String,
    },
    Keychain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::usage) struct Input {
    pub route: Route,
}

impl Route {
    pub(in crate::usage) fn parse(input: &Value, claude: bool) -> Result<Self> {
        // Optional strings in the source schema are not nullable.
        if let Some(route) = input.get("route") {
            for key in ["service", "account"] {
                optional_string(route, key)?;
            }
        }
        let Input { route } = Input::deserialize(input)?;
        let valid = match &route {
            Self::Claude { path } => claude && !path.is_empty(),
            Self::Codex { path } | Self::Opencode { path } => !claude && !path.is_empty(),
            Self::Pi { path } => !path.is_empty(),
            Self::Keychain { .. } => claude,
        };
        ensure!(valid, "Invalid usage credential route");
        Ok(route)
    }
    pub(in crate::usage) fn input(&self) -> Value {
        serde_json::json!({"route": self})
    }
    pub(in crate::usage) fn fallback_key(&self) -> String {
        super::super::common::hash_account_key(
            &serde_json::to_string(self).expect("serializable route"),
        )
    }
    pub(in crate::usage) fn store(&self) -> &'static str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Codex { .. } => "codex",
            Self::Opencode { .. } => "opencode",
            Self::Pi { .. } => "pi",
            Self::Keychain { .. } => "keychain",
        }
    }
    pub(in crate::usage) fn harness(&self) -> &'static str {
        match self {
            Self::Claude { .. } | Self::Keychain { .. } => "Claude",
            Self::Codex { .. } => "Codex",
            Self::Opencode { .. } => "OpenCode",
            Self::Pi { .. } => "Pi",
        }
    }
}

pub(in crate::usage) fn scope_env<'a>(
    rt: &'a UsageRuntime,
    scope: &'a UsageScope,
) -> (&'a BTreeMap<String, String>, PathBuf) {
    if scope.provider.is_none() {
        return (&rt.env, rt.home.clone());
    }
    let home = nonempty(&scope.env, "HOME")
        .or_else(|| nonempty(&scope.env, "USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| rt.home.clone());
    (&scope.env, home)
}
pub(in crate::usage) fn nonempty<'a>(
    env: &'a BTreeMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|s| !s.is_empty())
}
// Node path.join appends absolute-looking later components rather than replacing the root.
pub(in crate::usage) fn join(base: &Path, part: impl AsRef<Path>) -> PathBuf {
    let mut result = PathBuf::new();
    for (component, base_component) in base
        .components()
        .map(|c| (c, true))
        .chain(part.as_ref().components().map(|c| (c, false)))
    {
        match component {
            Component::RootDir | Component::Prefix(_) if base_component => {
                result.push(component.as_os_str())
            }
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if result.file_name().is_some_and(|s| s != "..") {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            Component::Normal(s) => result.push(s),
        }
    }
    result
}
fn resolve(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        join(path, "")
    } else {
        join(&std::env::current_dir().unwrap_or_default(), path)
    }
}
pub(in crate::usage) fn pi_path(home: &Path, env: &BTreeMap<String, String>) -> PathBuf {
    match env
        .get("PI_CODING_AGENT_DIR")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        None => join(home, ".pi/agent/auth.json"),
        Some("~") => join(home, "auth.json"),
        Some(s) => s
            .strip_prefix("~/")
            .map(|s| join(home, s))
            .unwrap_or_else(|| resolve(s))
            .join("auth.json"),
    }
}

pub(in crate::usage) struct OAuth {
    pub access: String,
    pub account_id: Option<String>,
    pub expires: Option<f64>,
}
fn oauth(value: &Value) -> Result<OAuth> {
    ensure!(value.is_object(), "Invalid OAuth credential");
    let access = value
        .get("access")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing OAuth access token"))?
        .to_owned();
    let account_id = optional_string(value, "accountId")?.map(str::to_owned);
    let expires = match value.get("expires") {
        None => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(|| anyhow::anyhow!("Invalid OAuth expiry"))?,
        ),
    };
    Ok(OAuth {
        access,
        account_id,
        expires,
    })
}
pub(in crate::usage) async fn read_harness(route: &Route, provider: &'static str) -> Option<OAuth> {
    match route {
        Route::Pi { path } | Route::Opencode { path } => {
            let json = read_json(path).await?;
            let key = if matches!(route, Route::Opencode { .. }) {
                "openai"
            } else {
                provider
            };
            let value = json.get(key)?;
            if value.get("type").and_then(Value::as_str) != Some("oauth") {
                return None;
            }
            oauth(value).ok()
        }
        _ => None,
    }
}
pub(in crate::usage) async fn read_json(path: impl AsRef<Path>) -> Option<Value> {
    super::super::common::read_json(path.as_ref())
        .await
        .ok()
        .flatten()
}
pub(in crate::usage) fn optional_string<'a>(
    value: &'a Value,
    key: &str,
) -> Result<Option<&'a str>> {
    match value.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => bail!("Invalid string field {key}"),
    }
}
pub(in crate::usage) fn nullable_string<'a>(
    value: &'a Value,
    key: &str,
) -> Result<Option<&'a str>> {
    if value.get(key).is_some_and(Value::is_null) {
        Ok(None)
    } else {
        optional_string(value, key)
    }
}
// Zod's number coercion uses JavaScript Number, including null, booleans and numeric strings.
pub(in crate::usage) fn number(value: &Value) -> Result<f64> {
    let n = match value {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                0.0
            } else if let Some(s) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u64::from_str_radix(s, 16)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN)
            } else if let Some(s) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
                u64::from_str_radix(s, 2)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN)
            } else if let Some(s) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
                u64::from_str_radix(s, 8)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN)
            } else {
                s.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Array(v) if v.is_empty() => 0.0,
        Value::Array(v) if v.len() == 1 => match &v[0] {
            Value::String(_) | Value::Number(_) | Value::Null | Value::Array(_) => {
                return number(&v[0]);
            }
            _ => f64::NAN,
        },
        _ => f64::NAN,
    };
    ensure!(n.is_finite(), "Invalid API number");
    Ok(n)
}
pub(in crate::usage) fn optional_number(
    value: &Value,
    key: &str,
    nullable: bool,
) -> Result<Option<f64>> {
    match value.get(key) {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(v) => number(v).map(Some),
    }
}
pub(in crate::usage) fn nullable_object<'a>(
    value: &'a Value,
    key: &str,
) -> Result<Option<&'a Value>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) if v.is_object() => Ok(Some(v)),
        _ => bail!("Invalid object field {key}"),
    }
}
pub(in crate::usage) fn iso_ms(ms: f64) -> Result<String> {
    ensure!(ms.is_finite() && ms.abs() <= 8.64e15, "Invalid timestamp");
    let date = chrono::DateTime::from_timestamp_millis(ms.trunc() as i64)
        .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;
    Ok(date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
pub(in crate::usage) async fn response_text(
    rt: &UsageRuntime,
    response: reqwest::Response,
) -> Result<String> {
    tokio::select! {
        biased;
        _ = rt.cancel.cancelled() => bail!("Usage request cancelled"),
        result = response.text() => Ok(result?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_reject_credential_fields() {
        assert!(Route::parse(&serde_json::json!({"accessToken":"secret"}), true).is_err());
        assert!(
            Route::parse(
                &serde_json::json!({"route":{"store":"pi","path":"/auth","token":"secret"}}),
                false
            )
            .is_err()
        );
    }
    #[test]
    fn pi_expands_tilde_for_agent_dir_override() {
        let home = Path::new("/fixture");
        let env = BTreeMap::from([("PI_CODING_AGENT_DIR".into(), "~/work".into())]);
        assert_eq!(pi_path(home, &env), Path::new("/fixture/work/auth.json"));
    }
}
