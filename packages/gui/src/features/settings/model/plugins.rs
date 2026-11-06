//! Plugin management rules from the original `plugins-page-state.ts` and
//! `formatPluginInstallation` (daemon CLI `identity`).

use serde_json::Value;

use crate::features::settings::json::{get, str_of};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageState {
    Offline,
    Unsupported,
    Loading,
    Error,
    Empty,
    Ready,
}

pub fn resolve_page(connected: bool, supported: bool, loading: bool, error: bool, count: usize) -> PageState {
    if !connected {
        PageState::Offline
    } else if !supported {
        PageState::Unsupported
    } else if loading {
        PageState::Loading
    } else if error {
        PageState::Error
    } else if count == 0 {
        PageState::Empty
    } else {
        PageState::Ready
    }
}

fn registry_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    if host.is_empty() { None } else { Some(host.to_string()) }
}

/// `formatPluginIdentity` over the native daemon's installation kinds.
pub fn identity(identity: &Value) -> String {
    if str_of(identity, "kind") == "directory" {
        return str_of(identity, "path").to_string();
    }
    let registry = get(identity, "registry");
    if registry.is_object()
        && let Some(host) = registry_host(str_of(registry, "url"))
    {
        return format!("{host}/{}", str_of(registry, "id"));
    }
    let source = match str_of(identity, "kind") {
        "cargo" => format!("cargo:{}", str_of(identity, "crateName")),
        "release" => format!("release:{}", str_of(identity, "origin")),
        "npm" => format!("npm:{}", str_of(identity, "packageName")),
        "git" => format!("git:{}", str_of(identity, "remote")),
        _ => return String::new(),
    };
    let path = str_of(identity, "pluginPath");
    if path.is_empty() || path == "." { source } else { format!("{source}:{path}") }
}

/// `formatPluginInstallation`: identity · revision (git revisions shortened).
pub fn installation(installation: &Value) -> String {
    let id = get(installation, "identity");
    let label = identity(id);
    let revision = str_of(installation, "currentRevision");
    let revision = if str_of(id, "kind") == "git" { revision.chars().take(12).collect::<String>() } else { revision.to_string() };
    if revision.is_empty() { label } else { format!("{label} · {revision}") }
}
