//! Remote parsing and forge web pages for the Open-in menu (original
//! protocol git-remote.ts, forge-manifest.ts cloud hosts, git/forge-url.ts).

/// protocol git-remote.ts `normalizeHost`.
pub fn normalize_git_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

fn normalize_remote_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_matches('/');
    let normalized = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if normalized.is_empty() { None } else { Some(normalized.to_owned()) }
}

fn valid_remote_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| edge(b) || b == b'.' || b == b'_' || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLocation {
    pub host: String,
    pub path: String,
}

/// protocol git-remote.ts `parseGitRemoteLocation`: the host and repository
/// path of an scp-style, ssh, http or https remote.
pub fn parse_git_remote_location(remote_url: &str) -> Option<RemoteLocation> {
    let trimmed = remote_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains("://") {
        // `^[^@]+@([^:]+):(.+)$`
        let (user, rest) = trimmed.split_once('@')?;
        if user.is_empty() || rest.contains('@') {
            return None;
        }
        let (host, path) = rest.split_once(':')?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        let host = normalize_git_host(host);
        let path = normalize_remote_path(path)?;
        return valid_remote_host(&host).then_some(RemoteLocation { host, path });
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "https" | "http" | "ssh") {
        return None;
    }
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host_port.split(':').next().unwrap_or("");
    let path = path.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(path)?;
    let host = normalize_git_host(host);
    let path = normalize_remote_path(&decoded)?;
    valid_remote_host(&host).then_some(RemoteLocation { host, path })
}

/// The http(s) port of a remote URL, when it names one explicitly.
fn explicit_http_port(remote_url: &str) -> Option<u16> {
    let (scheme, rest) = remote_url.trim().split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "https" | "http") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    host_port.rsplit_once(':')?.1.parse().ok()
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// protocol forge-manifest.ts `FORGE_DEFINITIONS` cloud hosts.
pub const FORGE_CLOUD_HOSTS: [(&str, &[&str]); 4] = [
    ("github", &["github.com", "ssh.github.com"]),
    ("gitlab", &["gitlab.com"]),
    ("gitea", &["gitea.com"]),
    ("codeberg", &["codeberg.org"]),
];

fn cloud_hosts(forge: &str) -> &'static [&'static str] {
    FORGE_CLOUD_HOSTS
        .iter()
        .find(|(id, _)| *id == forge)
        .map_or(&[], |(_, hosts)| *hosts)
}

/// git/forge.ts `forgeFromRemoteUrl`: a remote on a forge's cloud host.
pub fn forge_from_remote_url(remote_url: Option<&str>) -> Option<&'static str> {
    let host = parse_git_remote_location(remote_url?)?.host;
    FORGE_CLOUD_HOSTS
        .iter()
        .find(|(_, hosts)| hosts.iter().any(|h| normalize_git_host(h) == host))
        .map(|(forge, _)| *forge)
}

struct UrlGrammar {
    tree: &'static str,
    blob: &'static str,
    gitlab_lines: bool,
}

fn url_grammar(forge: &str) -> Option<UrlGrammar> {
    match forge {
        "github" => Some(UrlGrammar { tree: "/tree/", blob: "/blob/", gitlab_lines: false }),
        "gitlab" => Some(UrlGrammar { tree: "/-/tree/", blob: "/-/blob/", gitlab_lines: true }),
        "gitea" | "forgejo" | "codeberg" => Some(UrlGrammar {
            tree: "/src/branch/",
            blob: "/src/branch/",
            gitlab_lines: false,
        }),
        _ => None,
    }
}

/// `Uri.encodeComponent` (RFC 3986 unreserved plus `!'()*`).
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn encode_path(path: &str) -> String {
    path.split('/').map(encode_component).collect::<Vec<_>>().join("/")
}

/// git/forge-url.ts `normalizeBlobPath`: workspace relative without `.`
/// segments; `None` when it escapes the workspace.
fn blob_path(path: &str) -> Option<String> {
    let replaced = path.trim().replace('\\', "/");
    let trimmed = replaced.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    if segments.is_empty() { None } else { Some(segments.join("/")) }
}

/// git/forge-url.ts `buildForgeBranchTreeUrl` / `buildForgeBlobUrl`: the web
/// page of `branch` (or of `path` on it) on the remote's own host, so
/// self-hosted instances link correctly; cloud SSH aliases use the web host.
pub fn forge_web_url(
    forge: &str,
    remote_url: Option<&str>,
    branch: Option<&str>,
    path: Option<&str>,
    line_start: Option<i64>,
    line_end: Option<i64>,
) -> Option<String> {
    let grammar = url_grammar(forge)?;
    let remote_url = remote_url?;
    let location = parse_git_remote_location(remote_url)?;
    let name = branch.map(str::trim).unwrap_or("");
    if name.is_empty() || name == "HEAD" {
        return None;
    }
    if location.path.split('/').any(|segment| segment == "..") {
        return None;
    }
    let cloud: Vec<String> = cloud_hosts(forge).iter().map(|h| normalize_git_host(h)).collect();
    let is_cloud = cloud.contains(&location.host);
    let mut authority = if is_cloud { cloud[0].clone() } else { location.host.clone() };
    // A self-hosted http(s) origin keeps its port: the web UI shares it.
    if !is_cloud && let Some(port) = explicit_http_port(remote_url) {
        authority = format!("{authority}:{port}");
    }
    let base = format!("https://{authority}/{}", location.path);
    let Some(path) = path else {
        return Some(format!("{base}{}{}", grammar.tree, encode_path(name)));
    };
    let file = blob_path(path)?;
    let mut url = format!("{base}{}{}/{}", grammar.blob, encode_path(name), encode_path(&file));
    if let Some(start) = line_start.filter(|start| *start > 0) {
        url.push_str(&match line_end {
            Some(end) if end > start && grammar.gitlab_lines => format!("#L{start}-{end}"),
            Some(end) if end > start => format!("#L{start}-L{end}"),
            _ => format!("#L{start}"),
        });
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_pages_use_remote_host_cloud_hosts_and_grammars() {
        assert_eq!(
            forge_web_url("github", Some("git@ssh.github.com:me/repo.git"), Some("feat/x"), None, None, None).as_deref(),
            Some("https://github.com/me/repo/tree/feat/x")
        );
        assert_eq!(
            forge_web_url("gitlab", Some("https://git.example.com:8443/team/app.git"), Some("main"), Some("src/a b.rs"), Some(3), Some(5)).as_deref(),
            Some("https://git.example.com:8443/team/app/-/blob/main/src/a%20b.rs#L3-5")
        );
        assert_eq!(
            forge_web_url("codeberg", Some("https://codeberg.org/me/repo"), Some("main"), Some("./x/../y.md"), None, None).as_deref(),
            Some("https://codeberg.org/me/repo/src/branch/main/y.md")
        );
        assert_eq!(forge_web_url("github", Some("git@github.com:me/repo"), Some("HEAD"), None, None, None), None);
        assert_eq!(forge_web_url("github", Some("git@github.com:me/repo"), Some("m"), Some("../x"), None, None), None);
        assert_eq!(forge_web_url("bitbucket", Some("git@github.com:me/repo"), Some("m"), None, None, None), None);
    }

    #[test]
    fn remote_forge_is_detected_from_cloud_hosts_only() {
        assert_eq!(forge_from_remote_url(Some("git@github.com:a/b.git")), Some("github"));
        assert_eq!(forge_from_remote_url(Some("https://GitLab.com./a/b")), Some("gitlab"));
        assert_eq!(forge_from_remote_url(Some("https://git.example.com/a/b")), None);
        assert_eq!(forge_from_remote_url(Some("file:///a/b")), None);
    }
}
