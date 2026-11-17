use super::GitService;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct HeadCache(Mutex<HashMap<PathBuf, (SystemTime, u64, String)>>);

impl HeadCache {
    async fn read(&self, path: &Path) -> Option<String> {
        let metadata = tokio::fs::metadata(path).await.ok()?;
        let stamp = metadata.modified().ok()?;
        let mut cache = self.0.lock().await;
        if let Some((modified, len, value)) = cache.get(path)
            && *modified == stamp
            && *len == metadata.len()
        {
            return Some(value.clone());
        }
        let value = tokio::fs::read_to_string(path).await.ok()?;
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(path.to_owned(), (stamp, metadata.len(), value.clone()));
        Some(value)
    }

    async fn facts(&self, cwd: &Path) -> Value {
        for root in cwd.ancestors() {
            let marker = root.join(".git");
            let Ok(metadata) = tokio::fs::metadata(&marker).await else {
                continue;
            };
            let dir = if metadata.is_dir() {
                marker
            } else {
                let Some(marker) = self.read(&marker).await else {
                    break;
                };
                let Some(dir) = marker.trim().strip_prefix("gitdir:").map(str::trim) else {
                    break;
                };
                root.join(dir)
            };
            let Some(head) = self.read(&dir.join("HEAD")).await else {
                break;
            };
            let mut branch = head
                .trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned);
            // During a rebase HEAD is detached, but the UI has always shown the original branch.
            if branch.is_none() {
                for name in ["rebase-merge/head-name", "rebase-apply/head-name"] {
                    if let Some(name) = self.read(&dir.join(name)).await {
                        branch = Some(name.trim().trim_start_matches("refs/heads/").to_owned());
                        break;
                    }
                }
            }
            return json!({"isGit":true,"currentBranch":branch,"repoRoot":root,"gitDir":dir});
        }
        json!({"isGit":false,"currentBranch":null,"repoRoot":null})
    }
}

impl GitService {
    /// Branch-only directory projection: no observation, Git process, or status inventory.
    pub async fn head_facts(&self, cwd: &Path) -> Value {
        self.head_cache.facts(cwd).await
    }

    /// Placement and project identity without a status scan. Config includes are
    /// left to Git's effective-config read when a checkout is actually presented.
    pub async fn placement_facts(&self, cwd: &Path) -> anyhow::Result<Value> {
        let mut facts = self.head_facts(cwd).await;
        if facts["isGit"] != true {
            return Ok(facts);
        }
        let dir = Path::new(facts["gitDir"].as_str().unwrap());
        let common = match self.head_cache.read(&dir.join("commondir")).await {
            Some(common) => dir.join(common.trim()),
            None => dir.to_owned(),
        };
        let common = crate::projects::worktrees::normalize_path(&common).await?;
        let main = if common.file_name().is_some_and(|name| name == ".git") {
            common.parent().unwrap_or(&common)
        } else {
            &common
        };
        let env = self.environment()?;
        let variable = |name: &str| env.get(name).cloned().or_else(|| std::env::var(name).ok());
        let mut paths = Vec::new();
        if variable("GIT_CONFIG_NOSYSTEM").is_none_or(|value| value == "0") {
            paths.push(
                variable("GIT_CONFIG_SYSTEM")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/etc/gitconfig")),
            );
        }
        if let Some(global) = variable("GIT_CONFIG_GLOBAL") {
            paths.push(PathBuf::from(global));
        } else if let Some(home) = dirs::home_dir() {
            let xdg = variable("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            paths.push(xdg.join("git/config"));
            paths.push(home.join(".gitconfig"));
        }
        paths.push(common.join("config"));
        let mut remote = None;
        let mut complete =
            variable("GIT_CONFIG_COUNT").is_none() && variable("GIT_CONFIG_PARAMETERS").is_none();
        let mut worktree_config = false;
        for path in paths {
            if let Some(config) = self.head_cache.read(&path).await {
                match origin_url(&config) {
                    Ok(Some(url)) => remote = Some(url),
                    Ok(None) => {}
                    Err(()) => complete = false,
                }
                if path == common.join("config") {
                    let mut extensions = false;
                    for line in config.lines().map(str::trim) {
                        if line.starts_with('[') {
                            extensions = line.eq_ignore_ascii_case("[extensions]");
                        } else if extensions {
                            let (key, value) = line.split_once('=').unwrap_or((line, "true"));
                            if key.trim().eq_ignore_ascii_case("worktreeConfig") {
                                worktree_config = matches!(
                                    value.trim().to_ascii_lowercase().as_str(),
                                    "true" | "yes" | "on" | "1"
                                );
                            }
                        }
                    }
                }
            } else if !matches!(tokio::fs::metadata(&path).await, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                complete = false;
            }
        }
        if worktree_config
            && let Some(config) = self.head_cache.read(&dir.join("config.worktree")).await
        {
            match origin_url(&config) {
                Ok(Some(url)) => remote = Some(url),
                Ok(None) => {}
                Err(()) => complete = false,
            }
        }
        facts["worktreeRoot"] = facts["repoRoot"].clone();
        facts["mainRepoRoot"] = json!(main);
        facts["remoteUrl"] = json!(remote);
        facts["remoteConfigComplete"] = json!(complete);
        if let Some(snapshot) = self.cached_snapshot(cwd).await {
            facts["isPaseoOwnedWorktree"] = snapshot["git"]["isPaseoOwnedWorktree"].clone();
        }
        Ok(facts)
    }

    pub async fn directory_snapshot(&self, cwd: &Path) -> Value {
        let head = self.head_facts(cwd).await;
        if head["isGit"] != true {
            return json!({"git":super::facts::non_git(cwd),"forge":{
                "featuresEnabled":false,"pullRequest":null,"authState":"no_remote","error":null
            }});
        }
        let mut snapshot = self.cached_snapshot(cwd).await.unwrap_or_else(|| json!({
            "git":super::facts::non_git(cwd),
            "forge":{"featuresEnabled":false,"pullRequest":null,"authState":"no_remote","error":null}
        }));
        for key in ["isGit", "currentBranch", "repoRoot"] {
            snapshot["git"][key] = head[key].clone();
        }
        snapshot
    }
}

/// Parse Git's ordinary generated config, including quoting and continuations.
/// Includes can change precedence conditionally; never replace a persisted
/// project identity with a guessed effective remote in that case.
fn origin_url(config: &str) -> Result<Option<String>, ()> {
    let mut origin = false;
    let mut remote = None;
    let mut logical = String::new();
    for line in config.lines() {
        logical.push_str(line);
        if logical.ends_with('\\') && !logical.ends_with("\\\\") {
            logical.pop();
            continue;
        }
        let line = logical.trim();
        if line.starts_with('[') {
            let section = line
                .split(']')
                .next()
                .ok_or(())?
                .trim_start_matches('[')
                .trim();
            let lower = section.to_ascii_lowercase();
            if lower == "include"
                || lower.starts_with("includeif ")
                || lower.starts_with("includeif.")
            {
                return Err(());
            }
            origin = section
                .split_once(char::is_whitespace)
                .is_some_and(|(name, subsection)| {
                    name.eq_ignore_ascii_case("remote") && subsection.trim() == "\"origin\""
                })
                || section.eq_ignore_ascii_case("remote.origin");
        } else if origin
            && !line.starts_with(['#', ';'])
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let mut result = String::new();
            let mut quoted = false;
            let mut escaped = false;
            for character in value.trim().chars() {
                if escaped {
                    result.push(match character {
                        'n' => '\n',
                        't' => '\t',
                        'b' => '\u{0008}',
                        '\\' => '\\',
                        '"' => '"',
                        _ => return Err(()),
                    });
                    escaped = false;
                } else {
                    match character {
                        '\\' => escaped = true,
                        '"' => quoted = !quoted,
                        '#' | ';' if !quoted => break,
                        character => result.push(character),
                    }
                }
            }
            if quoted || escaped {
                return Err(());
            }
            remote = Some(result.trim_end().to_owned());
        }
        logical.clear();
    }
    if !logical.is_empty() {
        return Err(());
    }
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn head_cache_resolves_gitdir_and_refreshes_branch_without_git() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("checkout");
        let private = temp.path().join("private");
        tokio::fs::create_dir_all(root.join("nested")).await?;
        tokio::fs::create_dir_all(&private).await?;
        tokio::fs::write(root.join(".git"), "gitdir: ../private\n").await?;
        tokio::fs::write(private.join("HEAD"), "ref: refs/heads/main\n").await?;
        let cache = HeadCache::default();
        assert_eq!(
            cache.facts(&root.join("nested")).await["currentBranch"],
            "main"
        );
        tokio::fs::write(private.join("HEAD"), "ref: refs/heads/longer-branch\n").await?;
        assert_eq!(cache.facts(&root).await["currentBranch"], "longer-branch");
        tokio::fs::write(
            private.join("HEAD"),
            "0123456789012345678901234567890123456789\n",
        )
        .await?;
        let detached = cache.facts(&root).await;
        assert_eq!(detached["isGit"], true);
        assert!(detached["currentBranch"].is_null());
        tokio::fs::remove_file(root.join(".git")).await?;
        tokio::fs::create_dir(root.join(".git")).await?;
        tokio::fs::write(root.join(".git/HEAD"), "ref: refs/heads/normal\n").await?;
        assert_eq!(cache.facts(&root).await["currentBranch"], "normal");
        assert_eq!(
            origin_url("[REMOTE  \"origin\"]\n url = \"ssh://host/repo\" # note\n"),
            Ok(Some("ssh://host/repo".into()))
        );
        assert!(origin_url("[include]\npath = elsewhere\n").is_err());
        Ok(())
    }
}
