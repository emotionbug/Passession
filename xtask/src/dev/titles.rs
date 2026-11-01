use anyhow::{Context, Result, bail, ensure};
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Titles {
    paseo: String,
    codex: String,
}
#[derive(Debug, PartialEq)]
enum Target {
    Paseo,
    Codex,
}
fn resolve<'a>(
    paseo: &'a str,
    codex: &'a str,
    previous: Option<&Titles>,
) -> Option<(Target, &'a str)> {
    if paseo == codex {
        return None;
    }
    let Some(previous) = previous else {
        return (!codex.is_empty()).then_some((Target::Paseo, codex));
    };
    if !codex.is_empty() && codex != previous.codex {
        Some((Target::Paseo, codex))
    } else if !paseo.is_empty() && paseo != previous.paseo {
        Some((Target::Codex, paseo))
    } else {
        None
    }
}
fn index(text: &str) -> Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        thread_name: String,
    }
    let mut names = BTreeMap::new();
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) if lines.peek().is_none() => break,
            Err(error) => return Err(error.into()),
        };
        let entry: Entry = serde_json::from_value(value)?;
        names.insert(entry.id, entry.thread_name.trim().to_owned());
    }
    Ok(names)
}
fn optional(path: &Path, fallback: &str) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fallback.into()),
        Err(error) => Err(error.into()),
    }
}
fn homes() -> Result<(PathBuf, PathBuf)> {
    let home = super::user_home()?;
    Ok((
        std::env::var_os("PASEO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".paseo")),
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex")),
    ))
}
fn fingerprint(paseo: &Path, codex: &Path) -> Result<String> {
    fn visit(path: &Path, titles: &mut Vec<(String, String, String)>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(&entry.path(), titles)?;
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "json") {
                let text = optional(&entry.path(), "null")?;
                let agent: Value = serde_json::from_str(&text)?;
                if agent["provider"] == "codex" {
                    if let (Some(id), Some(session)) = (
                        agent["id"].as_str(),
                        agent
                            .pointer("/persistence/sessionId")
                            .and_then(Value::as_str),
                    ) {
                        titles.push((
                            id.into(),
                            session.into(),
                            agent["title"].as_str().unwrap_or("").into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    let mut titles = Vec::new();
    visit(&paseo.join("agents"), &mut titles)?;
    titles.sort();
    Ok(serde_json::to_string(
        &json!({"titles": titles,"codex":index(&optional(&codex.join("session_index.jsonl"), "")?)?}),
    )?)
}
async fn set_codex_names(
    updates: &[(String, String)],
    home: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let binary = std::env::var_os("CODEX_BIN").unwrap_or_else(|| "codex".into());
    let mut child = Command::new(binary)
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Starting Codex title service")?;
    let mut stdin = child.stdin.take().context("Missing Codex stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Missing Codex stdout")?).lines();
    let work = async {
        let mut requests = vec![(
            "initialize",
            json!({"clientInfo":{"name":"paseo_sessions_titles","version":"1.0.0"}}),
        )];
        requests.extend(
            updates
                .iter()
                .map(|(id, title)| ("thread/name/set", json!({"threadId":id,"name":title}))),
        );
        for (offset, (method, params)) in requests.into_iter().enumerate() {
            let id = offset + 1;
            let message =
                serde_json::to_string(&json!({"id":id,"method":method,"params":params}))? + "\n";
            stdin.write_all(message.as_bytes()).await?;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let line = stdout
                        .next_line()
                        .await?
                        .context("Codex title service exited")?;
                    let message: Value = serde_json::from_str(&line)?;
                    if message["id"].as_u64() != Some(id as u64) {
                        continue;
                    }
                    if let Some(error) = message.get("error") {
                        bail!(
                            "Codex {method} failed: {}",
                            error["message"].as_str().unwrap_or("RPC error")
                        );
                    }
                    ensure!(
                        message.get("result").is_some(),
                        "Codex response is missing result"
                    );
                    return Ok::<(), anyhow::Error>(());
                }
            })
            .await
            .context("Codex title RPC timed out")??;
            if offset == 0 {
                stdin
                    .write_all(b"{\"method\":\"initialized\",\"params\":{}}\n")
                    .await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::select! { result = work => result, _ = cancel.cancelled() => Err(anyhow::anyhow!("Title synchronization cancelled")) };
    drop(stdin);
    let cleanup = super::stop_child(&mut child).await;
    result.and(cleanup)
}
async fn sync(paseo: &Path, codex: &Path, cancel: &CancellationToken) -> Result<()> {
    let names = index(&optional(&codex.join("session_index.jsonl"), "")?)?;
    let state_path = paseo.join("session-title-sync.json");
    let previous: BTreeMap<String, Titles> = serde_json::from_str(&optional(&state_path, "{}")?)?;
    let target = if let Ok(url) = std::env::var("PASEO_TITLE_SYNC_URL") {
        DaemonTarget::Endpoint { host: url }
    } else {
        DaemonTarget::Instance {
            home: paseo.to_owned(),
        }
    };
    let mut options = ConnectOptions::new(target);
    options.home = Some(paseo.to_owned());
    options.password = if let Ok(password) = std::env::var("PASEO_PASSWORD") {
        Some(password)
    } else {
        let password = optional(&paseo.join("connection-password.txt"), "")?
            .trim()
            .to_owned();
        (!password.is_empty()).then_some(password)
    };
    options.client_id = Some("paseo-session-title-sync".into());
    options.timeout = Duration::from_secs(10);
    let client = DaemonClient::connect(options, cancel.child_token()).await?;
    let work = async {
        let mut cursor: Option<String> = None;
        let mut visited = BTreeSet::new();
        let mut next = BTreeMap::new();
        let mut updates = Vec::new();
        let mut updated = 0;
        loop {
            let page = client
                .fetch_agents(json!({"page":{"cursor":cursor,"limit":100}}))
                .await?;
            for entry in page["entries"]
                .as_array()
                .context("Agent response missing entries")?
            {
                let agent = &entry["agent"];
                if agent["provider"] != "codex" || !agent["parentAgentId"].is_null() {
                    continue;
                }
                let Some(thread) = agent
                    .pointer("/persistence/sessionId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let id = agent["id"].as_str().context("Agent response missing id")?;
                let key = format!("{id}:{thread}");
                let paseo_title = agent["title"].as_str().unwrap_or("").trim();
                let codex_title = names.get(thread).map(String::as_str).unwrap_or("");
                let mut titles = Titles {
                    paseo: paseo_title.into(),
                    codex: codex_title.into(),
                };
                if let Some((target, title)) = resolve(paseo_title, codex_title, previous.get(&key))
                {
                    match target {
                        Target::Paseo => {
                            client.update_agent(id, json!({"name":title})).await?;
                        }
                        Target::Codex => updates.push((thread.to_owned(), title.to_owned())),
                    }
                    titles = Titles {
                        paseo: title.into(),
                        codex: title.into(),
                    };
                    updated += 1;
                }
                next.insert(key, titles);
            }
            cursor = page
                .pointer("/pageInfo/nextCursor")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            match &cursor {
                None => break,
                Some(cursor) => ensure!(
                    visited.insert(cursor.clone()),
                    "Daemon returned a repeated agent cursor"
                ),
            }
        }
        set_codex_names(&updates, codex, cancel).await?;
        ensure!(!cancel.is_cancelled(), "Title synchronization cancelled");
        super::private_json(&state_path, &serde_json::to_value(next)?)?;
        if updated != 0 {
            println!("Synchronized {updated} session title(s).");
        }
        Ok::<(), anyhow::Error>(())
    };
    // Every operation below observes this token itself, including subprocess
    // teardown. Dropping the work future here would skip graceful Codex cleanup.
    let result = work.await;
    client.close().await;
    result
}
pub(super) async fn run(args: &[String], cancel: &CancellationToken) -> Result<()> {
    ensure!(
        args.is_empty() || args == ["--watch"],
        "Usage: cargo xtask maintenance codex-titles [--watch]"
    );
    let (paseo, codex) = homes()?;
    if args.is_empty() {
        return sync(&paseo, &codex, cancel).await;
    }
    let mut previous = None;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        // Title-only polling coalesces edits and works on every supported filesystem.
        // Retain the pre-sync snapshot: a rename during an RPC is checked again.
        let result = async {
            let snapshot = fingerprint(&paseo, &codex)?;
            if previous.as_ref() != Some(&snapshot) {
                sync(&paseo, &codex, cancel).await?;
                previous = Some(snapshot);
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let delay = if let Err(error) = result {
            if cancel.is_cancelled() {
                return Ok(());
            }
            eprintln!("Session title synchronization: {error}");
            Duration::from_secs(30)
        } else {
            Duration::from_millis(500)
        };
        tokio::select! { _ = cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_accepts_only_incomplete_final_records_and_latest_duplicate() {
        let names = index("{\"id\":\"a\",\"thread_name\":\"old\"}\n{\"id\":\"a\",\"thread_name\":\" new \"}\n{\"id\":").unwrap();
        assert_eq!(names["a"], "new");
        assert!(index("broken\n").is_err());
        assert!(index("{\"id\":4,\"thread_name\":\"name\"}").is_err());
    }
    #[test]
    fn title_conflicts_favor_explicit_codex_edit_not_activity() {
        let previous = Titles {
            paseo: "old".into(),
            codex: "old".into(),
        };
        assert_eq!(
            resolve("new paseo", "new codex", Some(&previous)),
            Some((Target::Paseo, "new codex"))
        );
        assert_eq!(
            resolve("new paseo", "old", Some(&previous)),
            Some((Target::Codex, "new paseo"))
        );
        assert_eq!(resolve("local", "", None), None);
        assert_eq!(
            resolve("local", "codex", None),
            Some((Target::Paseo, "codex"))
        );
        assert_eq!(resolve("old", "", Some(&previous)), None);
    }
}
