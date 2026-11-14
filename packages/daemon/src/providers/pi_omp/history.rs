use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};

use super::events::{Mapper, omp_task_completions, omp_task_status};
use crate::providers::api::{HistoryCoverage, SubagentHistoryRequest, subagent_row};

static TITLE_PREFIX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}[-:]\d{2}[-:]\d{2}").unwrap());
static LEGACY_TRANSCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(?:session|transcript)(?: file)?:\s*(/\S+\.jsonl)").unwrap()
});
static WINDOWS_PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[a-zA-Z]:[\\/]|^[/\\]{2}\?[/\\]|^\\{2}[^/\\]+[/\\][^/\\]+").unwrap()
});

pub struct History {
    pub descriptor: Value,
    pub timeline: Vec<Value>,
    pub subagent_events: Vec<Value>,
    pub messages: Vec<Value>,
    /// Stream envelopes retain replay timestamps, unlike initial timeline items.
    pub events: Vec<Value>,
    /// False for a torn/invalid record, missing branch/ancestor, or missing file.
    pub complete: bool,
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn record(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line.trim())
        .ok()
        .filter(Value::is_object)
}

fn iso(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn date_ms(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_f64().filter(|n| n.is_finite()) {
        return Some(n as i64);
    }
    let text = value.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .map(|d| d.timestamp_millis())
        .ok()
        .or_else(|| {
            DateTime::parse_from_rfc2822(text)
                .map(|d| d.timestamp_millis())
                .ok()
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc().timestamp_millis())
        })
}

pub fn replay_timestamp(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        let s = s.trim();
        return date_ms(&json!(s)).map(|_| s.to_owned());
    }
    let n = value.as_f64().filter(|n| n.is_finite())?;
    iso(if n > 1_000_000_000_000.0 {
        n
    } else {
        n * 1000.0
    } as i64)
}

pub fn message_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn descriptor_text(content: &Value) -> Option<String> {
    let text = message_text(content);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn model_id(provider: &Value, model: &Value) -> Option<String> {
    Some(format!("{}/{}", nonempty(provider)?, nonempty(model)?))
}

fn entry_model(family: &str, entry: &Value) -> Option<String> {
    match entry["type"].as_str() {
        Some("model_change") => model_id(&entry["provider"], &entry["modelId"]).or_else(|| {
            (family == "omp")
                .then(|| nonempty(&entry["model"]).map(str::to_owned))
                .flatten()
        }),
        Some("message") => model_id(&entry["message"]["provider"], &entry["message"]["model"]),
        _ => None,
    }
}

fn title(family: &str, entry: &Value) -> Option<String> {
    let value = match entry["type"].as_str() {
        Some("session_info") => &entry["name"],
        Some("title") if family == "omp" => &entry["title"],
        _ => return None,
    };
    nonempty(value).map(str::to_owned)
}

fn header(family: &str, chunk: &str) -> Option<Value> {
    let mut lines = chunk.lines();
    let valid = |e: &Value| {
        e["type"] == "session"
            && e["id"].as_str().is_some_and(|s| !s.is_empty())
            && e["cwd"].as_str().is_some_and(|s| !s.is_empty())
    };
    if family == "pi" {
        return record(lines.next()?).filter(valid);
    }
    lines.filter_map(record).find(valid)
}

async fn chunks(path: &Path) -> Result<(String, String, SystemTime)> {
    let mut file = fs::File::open(path).await?;
    let meta = file.metadata().await?;
    let mut head = vec![0; (meta.len().min(64 * 1024)) as usize];
    let count = file.read(&mut head).await?;
    head.truncate(count);
    let start = meta.len().saturating_sub(256 * 1024);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut tail = vec![0; (meta.len() - start) as usize];
    let count = file.read(&mut tail).await?;
    tail.truncate(count);
    Ok((
        String::from_utf8_lossy(&head).into_owned(),
        String::from_utf8_lossy(&tail).into_owned(),
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}

/// Descriptor parsing deliberately uses the same bounded head/tail windows as the providers.
pub async fn describe(family: &str, path: &Path) -> Result<Option<Value>> {
    let (head, tail, mtime) = match chunks(path).await {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(header) = header(family, &head) else {
        return Ok(None);
    };
    let (mut head_title, mut first, mut head_model, mut head_thinking) = (None, None, None, None);
    for (index, line) in head.lines().enumerate() {
        let Some(entry) = record(line) else { continue };
        head_title = title(family, &entry).or(head_title);
        head_model = entry_model(family, &entry).or(head_model);
        if entry["type"] == "thinking_level_change" {
            head_thinking = nonempty(&entry["thinkingLevel"])
                .map(str::to_owned)
                .or(head_thinking);
        }
        if first.is_none() && entry["type"] == "message" && entry["message"]["role"] == "user" {
            first = descriptor_text(&entry["message"]["content"]);
        }
        if (head_title.is_some()
            && first.is_some()
            && head_model.is_some()
            && head_thinking.is_some())
            || (index + 1 >= 2000 && first.is_some())
        {
            break;
        }
    }
    let (mut tail_title, mut last, mut tail_model, mut tail_thinking) = (None, None, None, None);
    let (mut activity, mut fallback) = (None, None);
    for entry in tail.lines().rev().filter_map(record) {
        if tail_title.is_none() {
            tail_title = title(family, &entry);
        }
        if tail_model.is_none() {
            tail_model = entry_model(family, &entry);
        }
        if tail_thinking.is_none() && entry["type"] == "thinking_level_change" {
            tail_thinking = nonempty(&entry["thinkingLevel"]).map(str::to_owned);
        }
        let timestamp = date_ms(&entry["timestamp"]);
        if fallback.is_none() {
            fallback = timestamp;
        }
        if entry["type"] != "message" {
            continue;
        }
        if activity.is_none() {
            activity = timestamp;
        }
        if last.is_none() && entry["message"]["role"] == "user" {
            last = descriptor_text(&entry["message"]["content"]);
        }
    }
    let path_title = if family == "omp" {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| !TITLE_PREFIX.is_match(s))
            .filter(|s| !(s.len() >= 8 && s.bytes().all(|c| c.is_ascii_hexdigit())))
            .map(str::to_owned)
    } else {
        None
    };
    let model = tail_model.or(head_model);
    let thinking = tail_thinking.or(head_thinking);
    let mut config = json!({});
    if let Some(model) = &model {
        config["model"] = json!(model);
    }
    if let Some(thinking) = &thinking {
        config["thinkingOptionId"] = json!(thinking);
    }
    let mtime: DateTime<Utc> = mtime.into();
    Ok(Some(json!({
        "sessionId":header["id"], "sessionFile":path, "cwd":header["cwd"],
        "title":tail_title.or(head_title).or(path_title).or_else(|| first.clone()),
        "firstUserMessage":first, "lastUserMessage":last,
        "lastActivityAt":iso(activity.or(fallback).unwrap_or(mtime.timestamp_millis())),
        "model":model, "thinkingOptionId":thinking, "importConfig":config,
        "header":header
    })))
}

fn resolve_path(value: &str, base: &Path, home: &Path) -> PathBuf {
    let path = if value == "~" {
        home.to_owned()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        base.join(value)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn profile(env: &BTreeMap<String, String>) -> Result<Option<String>> {
    let value = env
        .get("OMP_PROFILE")
        .or_else(|| env.get("PI_PROFILE"))
        .map(|s| s.trim())
        .unwrap_or("");
    if value.is_empty() || value == "default" {
        return Ok(None);
    }
    let name = value.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (name.len() == 4
            && (name.starts_with("COM") || name.starts_with("LPT"))
            && name.as_bytes()[3].is_ascii_digit());
    if value.len() > 64
        || !value.as_bytes()[0].is_ascii_lowercase() && !value.as_bytes()[0].is_ascii_digit()
        || value.ends_with('.')
        || !value.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
        || reserved
    {
        bail!("Invalid OMP profile \"{value}\"");
    }
    Ok(Some(value.to_owned()))
}

async fn settings_dir(path: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(&fs::read(path).await.ok()?).ok()?;
    value["sessionDir"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

pub async fn sessions_dir(
    family: &str,
    env: &BTreeMap<String, String>,
    options: &Value,
) -> Result<PathBuf> {
    let mut env: BTreeMap<String, String> = std::env::vars()
        .chain(env.iter().map(|(k, v)| (k.clone(), v.clone())))
        .collect();
    if options["env"].is_object() {
        env.clear();
    }
    for source in [options.get("env"), options.pointer("/runtimeSettings/env")] {
        if let Some(source) = source.and_then(Value::as_object) {
            for (key, value) in source {
                if let Some(value) = value.as_str() {
                    env.insert(key.clone(), value.to_owned());
                }
            }
        }
    }
    let cwd = std::env::current_dir()?;
    let base = options["cwd"].as_str().map(Path::new).unwrap_or(&cwd);
    let home = options["homeDir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("Home directory is unavailable"))?;
    if let Some(dir) = options
        .get("sessionDir")
        .or_else(|| options.pointer("/providerOptions/sessionDir"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(resolve_path(dir, base, &home));
    }
    let profile = if family == "omp" {
        profile(&env)?
    } else {
        None
    };
    let agent = if profile.is_none() {
        env.get("PI_CODING_AGENT_DIR")
            .filter(|s| !s.trim().is_empty())
            .map(|s| resolve_path(s, &cwd, &home))
    } else {
        None
    };
    let agent = agent.unwrap_or_else(|| {
        let config = if family == "omp" {
            env.get("PI_CONFIG_DIR")
                .filter(|s| !s.is_empty())
                .map(String::as_str)
                .unwrap_or(".omp")
        } else {
            ".pi"
        };
        // Node path.join keeps an absolute-looking config suffix beneath home.
        let mut root = home.join(config.trim_start_matches('/'));
        if let Some(profile) = &profile {
            root = root.join("profiles").join(profile);
        }
        root.join("agent")
    });
    if let Some(dir) = env
        .get("PI_CODING_AGENT_SESSION_DIR")
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(resolve_path(dir, base, &home));
    }
    let global = settings_dir(&agent.join("settings.json")).await;
    let local = if let Some(cwd) = options["cwd"].as_str() {
        settings_dir(
            &Path::new(cwd)
                .join(if family == "omp" { ".omp" } else { ".pi" })
                .join("settings.json"),
        )
        .await
    } else {
        None
    };
    if let Some(dir) = local.or(global) {
        return Ok(resolve_path(&dir, base, &home));
    }
    if family == "omp"
        && cfg!(any(target_os = "linux", target_os = "macos"))
        && (profile.is_some()
            || env
                .get("PI_CODING_AGENT_DIR").is_none_or(|s| s.trim().is_empty()))
        && let Some(xdg) = env.get("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            let mut root = Path::new(xdg).join("omp");
            if let Some(profile) = &profile {
                root = root.join("profiles").join(profile);
            }
            if fs::try_exists(&root).await.unwrap_or(false) {
                return Ok(root.join("sessions"));
            }
        }
    Ok(agent.join("sessions"))
}

async fn files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_owned()];
    let mut out = Vec::new();
    while let Some(path) = pending.pop() {
        let Ok(mut dir) = fs::read_dir(&path).await else {
            continue;
        };
        let mut entries = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            entries.push(entry);
        }
        entries.sort_by_key(|e| e.file_name());
        // Reverse stack traversal then sort the final paths to match recursive readdir order.
        for entry in entries {
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".jsonl") {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn preview(value: &Value) -> Value {
    let text = value
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return Value::Null;
    }
    // JavaScript previews are bounded in UTF-16 code units, not UTF-8 bytes.
    let units: Vec<_> = text.encode_utf16().take(160).collect();
    json!(String::from_utf16_lossy(&units))
}

pub async fn discover(
    family: &str,
    env: &BTreeMap<String, String>,
    options: &Value,
) -> Result<Vec<Value>> {
    let root = sessions_dir(family, env, options).await?;
    let mut ranked = Vec::new();
    for file in files(&root).await {
        if let Ok(meta) = fs::metadata(&file).await
            && let Ok(mtime) = meta.modified() {
                ranked.push((file, mtime));
            }
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let limit = options["limit"].as_u64().unwrap_or(20) as usize;
    let scan = options["scanLimit"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or_else(|| limit.saturating_mul(40).max(400))
        .min(500);
    let cwd = options["cwd"].as_str().filter(|s| !s.is_empty());
    let canonical_cwd = if let Some(cwd) = cwd {
        fs::canonicalize(cwd).await.ok()
    } else {
        None
    };
    if options.get("scanLimit").is_some() || cwd.is_none() {
        ranked.truncate(scan);
    }
    let mut out = Vec::new();
    for (file, _) in ranked {
        let Some(desc) = describe(family, &file).await? else {
            continue;
        };
        if let Some(cwd) = cwd {
            let candidate = desc["cwd"].as_str().unwrap_or("");
            let same = equivalent_paths(candidate, cwd);
            if !same {
                let canonical_candidate = fs::canonicalize(candidate).await.ok();
                let canonical_match = canonical_candidate
                    .as_ref()
                    .is_some_and(|p| equivalent_paths(&p.to_string_lossy(), cwd))
                    || canonical_cwd
                        .as_ref()
                        .is_some_and(|p| equivalent_paths(candidate, &p.to_string_lossy()))
                    || (canonical_cwd.is_some() && canonical_candidate == canonical_cwd);
                if !canonical_match {
                    continue;
                }
            }
        }
        out.push(json!({"providerHandleId":file,"cwd":desc["cwd"],"title":desc["title"],"firstPromptPreview":preview(&desc["firstUserMessage"]),"lastPromptPreview":preview(if desc["lastUserMessage"].is_null() { &desc["firstUserMessage"] } else { &desc["lastUserMessage"] }),"lastActivityAt":desc["lastActivityAt"]}));
        if out.len() >= limit {
            break;
        }
    }
    out.sort_by(|a, b| {
        b["lastActivityAt"]
            .as_str()
            .cmp(&a["lastActivityAt"].as_str())
    });
    Ok(out)
}

pub async fn resolve_session_file(
    family: &str,
    handle: &str,
    env: &BTreeMap<String, String>,
    options: &Value,
) -> Result<Option<PathBuf>> {
    let handle = if family == "omp" {
        handle.trim()
    } else {
        handle
    };
    if handle.is_empty() {
        return Ok(None);
    }
    if fs::metadata(handle).await.is_ok_and(|m| m.is_file()) {
        return Ok(Some(PathBuf::from(handle)));
    }
    if family != "omp" {
        return Ok(None);
    }
    let root = sessions_dir(family, env, options).await?;
    let mut files = files(&root).await;
    let suffix = format!("_{handle}.jsonl");
    files.sort_by_key(|p| {
        !p.file_name()
            .is_some_and(|s| s.to_string_lossy().ends_with(&suffix))
    });
    for file in files {
        if let Ok((head, _, _)) = chunks(&file).await
            && header(family, &head).is_some_and(|h| h["id"] == handle) {
                return Ok(Some(file));
            }
    }
    Ok(None)
}

pub fn import_persistence(provider: &str, handle: &str, cwd: &str, stored_config: &Value) -> Value {
    let mut metadata = stored_config.as_object().cloned().unwrap_or_default();
    metadata.insert("provider".into(), json!(provider));
    metadata.insert("cwd".into(), json!(cwd));
    json!({"provider":provider,"sessionId":handle,"nativeHandle":handle,"metadata":metadata})
}

async fn active_entries_with_coverage(path: &Path, active_entry_id: Option<&str>) -> Result<(Vec<Value>, bool)> {
    let content = fs::read(path).await?;
    let text = String::from_utf8_lossy(&content);
    let mut complete = matches!(text, std::borrow::Cow::Borrowed(_));
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match record(line) {
            // OMP's title preamble is file metadata, not a session-tree entry.
            Some(entry) if entry["type"] == "title" && entry["title"].is_string()
                && entry.get("id").is_none() && entry.get("parentId").is_none()
                && entry.get("message").is_none() => {}
            Some(entry) if entry["id"].is_string() => entries.push(entry),
            Some(_) => complete = false,
            None => complete = false,
        }
    }
    let by_id: HashMap<_, _> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e["id"].as_str().map(|id| (id, i)))
        .collect();
    complete &= by_id.len() == entries.len();
    if let Some(id) = active_entry_id.filter(|id| !id.is_empty())
        && !by_id.contains_key(id) { return Ok((Vec::new(), false)); }
    let parents: HashSet<_> = entries
        .iter()
        .filter_map(|e| e["parentId"].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let mut current = active_entry_id
        .filter(|s| !s.is_empty())
        .and_then(|id| by_id.get(id).copied())
        .or_else(|| {
            entries
                .iter()
                .rposition(|e| !parents.contains(e["id"].as_str().unwrap_or("")))
        })
        .or_else(|| entries.len().checked_sub(1));
    let mut seen = HashSet::new();
    let mut chain = Vec::new();
    while let Some(index) = current {
        let entry = &entries[index];
        let id = entry["id"].as_str().unwrap_or("");
        if id.is_empty() || !seen.insert(id) {
            complete = false;
            break;
        }
        chain.push(entry.clone());
        current = match entry["parentId"].as_str().filter(|id| !id.is_empty()) {
            Some(parent) => {
                let next = by_id.get(parent).copied();
                complete &= next.is_some();
                next
            }
            None => None,
        };
    }
    chain.reverse();
    Ok((chain, complete))
}

fn fallback_message(role: &str, value: &Value) -> Value {
    let text = if let Some(text) = value["content"].as_str() {
        text.to_owned()
    } else if let Some(parts) = value["content"].as_array() {
        let text = parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            "Unsupported history record".into()
        } else {
            text
        }
    } else {
        value["text"]
            .as_str()
            .unwrap_or("Unsupported history record")
            .to_owned()
    };
    json!({"role":"custom","content":format!("[{role}] {text}")})
}

fn entry_message(family: &str, entry: &Value) -> Option<Value> {
    if let Some(role) = entry["message"]["role"].as_str() {
        let message = &entry["message"];
        if matches!(
            role,
            "user" | "assistant" | "toolResult" | "custom" | "bashExecution"
        ) {
            return Some(message.clone());
        }
        if family == "pi"
            || matches!(
                role,
                "system"
                    | "pythonExecution"
                    | "hookMessage"
                    | "branchSummary"
                    | "compactionSummary"
                    | "fileMention"
            )
        {
            return None;
        }
        if role == "developer"
            && let Some(content) = message["content"].as_array().filter(|c| c.len() == 1)
                && content[0]["type"] == "text"
                    && let Some(text) = content[0]["text"].as_str() {
                        let text = text.trim();
                        if text.ends_with("</system-reminder>")
                            && text.strip_prefix("<system-reminder").is_some_and(|s| {
                                s.starts_with('>')
                                    || s.chars().next().is_some_and(char::is_whitespace)
                            })
                        {
                            return None;
                        }
                    }
        return Some(fallback_message(role, message));
    }
    if entry["type"] == "custom_message" {
        if !entry["content"].is_string() && !entry["content"].is_array() {
            return None;
        }
        let mut message = json!({"role":"custom","content":entry["content"]});
        for key in ["id", "customType", "display", "details", "attribution"] {
            if let Some(value) = entry.get(key) {
                message[key] = value.clone();
            }
        }
        return Some(message);
    }
    let kind = entry["type"].as_str()?;
    if family == "pi"
        || matches!(
            kind,
            "session"
                | "session_init"
                | "system"
                | "title"
                | "title_change"
                | "custom"
                | "system_prompt"
                | "model_change"
                | "thinking_level_change"
                | "ttsr_injection"
                | "tool_execution"
        )
        || kind.starts_with("tool_execution_")
    {
        return None;
    }
    Some(fallback_message(kind, entry))
}

pub async fn load_with_metadata(
    family: &str,
    provider: &str,
    path: &Path,
    active_entry_id: Option<&str>,
    metadata: &Value,
) -> Result<History> {
    load_inner(
        family,
        provider,
        path,
        active_entry_id,
        true,
        metadata.get("bridgedTools"),
    )
    .await
}

pub async fn load_live(
    family: &str,
    provider: &str,
    path: &Path,
    active_entry_id: Option<&str>,
    metadata: &Value,
) -> Result<History> {
    load_inner(
        family,
        provider,
        path,
        active_entry_id,
        false,
        metadata.get("bridgedTools"),
    )
    .await
}

/// Pi appends every entry to an existing session file synchronously and resumes
/// it with the file's last entry as the leaf. A complete file ending at the live
/// runtime's leaf is therefore an authoritative replay of the live conversation,
/// including after a tree rewind. Stale, newer, torn or missing files are not.
pub async fn verified_pi_live(provider: &str, path: &Path, live_leaf: &Value) -> Result<Option<History>> {
    if !live_leaf.is_string() {
        return Ok(None);
    }
    let history = load_live("pi", provider, path, None, &Value::Null).await?;
    Ok((history.complete && history.descriptor["activeBranchEntryId"] == *live_leaf).then_some(history))
}

fn push_event(history: &mut History, event: Value) {
    if event["type"] == "timeline" {
        history.timeline.push(event["item"].clone());
    }
    if event["type"] == "provider_subagent" {
        history.subagent_events.push(event.clone());
    }
    history.events.push(event);
}

/// Reads only `path`. The children it spawned are announced by descriptor with a
/// `source` locator; a child transcript is read only when that child is opened.
async fn load_inner(
    family: &str,
    provider: &str,
    path: &Path,
    active_entry_id: Option<&str>,
    append_todo: bool,
    bridged_tools: Option<&Value>,
) -> Result<History> {
    let descriptor = describe(family, path).await?.unwrap_or(Value::Null);
    let (entries, complete) = match active_entries_with_coverage(path, active_entry_id).await {
        Ok(result) => result,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(empty_history(descriptor));
        }
        Err(error) => return Err(error),
    };
    Ok(project(
        family,
        provider,
        path,
        descriptor,
        &entries,
        complete,
        append_todo,
        bridged_tools,
        false,
    ))
}

fn empty_history(descriptor: Value) -> History {
    History {
        descriptor,
        timeline: Vec::new(),
        subagent_events: Vec::new(),
        messages: Vec::new(),
        events: Vec::new(),
        complete: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn project(
    family: &str,
    provider: &str,
    path: &Path,
    descriptor: Value,
    entries: &[Value],
    complete: bool,
    append_todo: bool,
    bridged_tools: Option<&Value>,
    // A child transcript, identified the way its live events are.
    child: bool,
) -> History {
    let mut history = empty_history(descriptor);
    history.complete = complete && history.descriptor.is_object();
    if history.descriptor.is_object() {
        history.descriptor["activeBranchEntryId"] = entries
            .last()
            .map(|e| e["id"].clone())
            .unwrap_or(Value::Null);
    }
    let mut mapper = Mapper::new(family, provider);
    if child {
        mapper = mapper.native_ids();
    }
    if let Some(tools) = bridged_tools {
        mapper.set_bridged_tools(tools);
    }
    for entry in entries {
        let timestamp = if family == "omp" {
            replay_timestamp(&entry["timestamp"])
        } else {
            None
        };
        if family == "omp" && entry["type"] == "compaction" {
            let mut item = json!({"type":"compaction","status":"completed"});
            if entry["tokensBefore"].is_number() {
                item["preTokens"] = entry["tokensBefore"].clone();
            }
            let mut event = json!({"type":"timeline","provider":provider,"item":item});
            if let Some(timestamp) = timestamp {
                event["timestamp"] = json!(timestamp);
            }
            push_event(&mut history, event);
            continue;
        }
        let Some(message) = entry_message(family, entry) else {
            continue;
        };
        let user_id = (message["role"] == "user")
            .then(|| entry["id"].as_str())
            .flatten();
        for mut event in mapper.replay_message(&message, user_id) {
            if event["type"] == "timeline" {
                if let Some(timestamp) = &timestamp {
                    event["timestamp"] = json!(timestamp);
                } else if let Some(object) = event.as_object_mut() {
                    object.remove("timestamp");
                }
            }
            push_event(&mut history, event);
        }
        history.messages.push(message);
    }
    if family == "omp" {
        let finished: HashMap<&str, String> = entries
            .iter()
            .filter(|e| e["message"]["role"] == "toolResult")
            .filter_map(|e| {
                Some((
                    e["message"]["toolCallId"].as_str()?,
                    replay_timestamp(&e["timestamp"])?,
                ))
            })
            .collect();
        for transcript in subagent_transcripts(&history.messages, path) {
            let timestamp = finished.get(transcript.tool_call.as_str()).cloned();
            let title = subagent_title(&transcript.title, None);
            let event = transcript.upsert(provider, transcript.status, &title, timestamp);
            push_event(&mut history, event);
        }
        if append_todo
            && let Some(todo) = todo_state(entries) {
                push_event(
                    &mut history,
                    json!({"type":"timeline","provider":provider,"item":todo}),
                );
            }
    } else {
        for child in mapper.drain_child_sessions() {
            if let Some(event) = pi_child_source(provider, &child) {
                push_event(&mut history, event);
            }
        }
    }
    history
}

/// Attaches the transcript file a Pi extension reported to the child's descriptor.
pub(super) fn pi_child_source(provider: &str, child: &Value) -> Option<Value> {
    let (Some(id), Some(file)) = (child["id"].as_str(), child["file"].as_str()) else {
        return None;
    };
    Some(json!({"type":"provider_subagent","provider":provider,
        "event":{"type":"upsert","id":id,"source":{"file":file}}}))
}

/// The opened child's rows, a title refreshed with its model, and descriptors for
/// the children it spawned. Only that child's transcript is read. A descriptor
/// stored before `source` existed is located again from the parent file alone.
pub async fn child_history(
    family: &str,
    provider: &str,
    request: &SubagentHistoryRequest,
) -> Result<Vec<Value>> {
    let source = if request.source.is_object() {
        request.source.clone()
    } else {
        let parent = request.persistence["nativeHandle"]
            .as_str()
            .or(request.persistence["sessionId"].as_str())
            .ok_or_else(|| anyhow!("Subagent parent has no native session file"))?;
        let parent = load_inner(family, provider, Path::new(parent), None, false, None).await?;
        parent
            .subagent_events
            .iter()
            .map(|event| &event["event"])
            .find(|event| event["id"] == request.child_id.as_str() && event["source"].is_object())
            .map(|event| event["source"].clone())
            .ok_or_else(|| {
                anyhow!(
                    "Subagent '{}' has no transcript in its parent session",
                    request.child_id
                )
            })?
    };
    if family == "omp" {
        omp_child_history(provider, request, &source).await
    } else {
        pi_child_history(provider, request, &source).await
    }
}

async fn omp_child_history(
    provider: &str,
    request: &SubagentHistoryRequest,
    source: &Value,
) -> Result<Vec<Value>> {
    let path = Path::new(
        source["path"]
            .as_str()
            .ok_or_else(|| anyhow!("OMP subagent '{}' has no transcript path", request.child_id))?,
    );
    let (entries, complete) = active_entries_with_coverage(path, None)
        .await
        .map_err(|error| {
            anyhow!(
                "OMP subagent transcript {} is unavailable: {error:#}",
                path.display()
            )
        })?;
    let model = entries
        .iter()
        .filter_map(|e| {
            if e["type"] == "model_change" {
                model_id(&e["provider"], &e["modelId"])
            } else if e["type"] == "message" && e["message"]["role"] == "assistant" {
                model_id(
                    &e["message"]["provider"],
                    e["message"]
                        .get("responseModel")
                        .filter(|v| !v.is_null())
                        .unwrap_or(&e["message"]["model"]),
                )
            } else {
                None
            }
        })
        .next_back();
    let descriptor = describe("omp", path).await?.unwrap_or(Value::Null);
    let child = project("omp", provider, path, descriptor, &entries, complete, false, None, true);
    let coverage = if child.complete {
        HistoryCoverage::Complete
    } else {
        HistoryCoverage::Unverified
    };
    let title = subagent_title(source["title"].as_str().unwrap_or(""), model.as_deref());
    let mut events = vec![json!({"type":"provider_subagent","provider":provider,
        "event":{"type":"upsert","id":request.child_id,"title":title}})];
    for mut event in child.events {
        if event["type"] == "timeline" {
            events.push(subagent_row(
                provider,
                &request.child_id,
                event["item"].take(),
                event.get("timestamp"),
                coverage,
            ));
        } else if event["type"] == "provider_subagent" {
            if event["event"]["parentSubagentId"].is_null() {
                event["event"]["parentSubagentId"] = json!(request.child_id);
            }
            events.push(event);
        }
    }
    Ok(events)
}

/// Complete only when every line of the file parsed and none was torn.
async fn pi_child_history(
    provider: &str,
    request: &SubagentHistoryRequest,
    source: &Value,
) -> Result<Vec<Value>> {
    let file = source["file"]
        .as_str()
        .ok_or_else(|| anyhow!("Pi subagent '{}' has no transcript file", request.child_id))?;
    let data = fs::read(file)
        .await
        .map_err(|error| anyhow!("Pi subagent transcript {file} is unavailable: {error}"))?;
    let end = data.iter().rposition(|b| *b == b'\n').map_or(0, |last| last + 1);
    let text = String::from_utf8_lossy(&data[..end]);
    let mut complete = end == data.len() && matches!(text, std::borrow::Cow::Borrowed(_));
    let mut mapper = Mapper::new("pi", provider).native_ids();
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Some(entry) = record(line) else {
            complete = false;
            continue;
        };
        if !entry["message"]["role"].is_string() {
            continue;
        }
        for event in mapper.replay_message(&entry["message"], None) {
            if event["type"] == "timeline" {
                let timestamp = Some(&entry["timestamp"]).filter(|t| truthy(t)).cloned();
                rows.push((limit_child_item(event["item"].clone()), timestamp));
            }
        }
    }
    let coverage = if complete {
        HistoryCoverage::Complete
    } else {
        HistoryCoverage::Unverified
    };
    Ok(rows
        .into_iter()
        .map(|(item, timestamp)| {
            subagent_row(provider, &request.child_id, item, timestamp.as_ref(), coverage)
        })
        .collect())
}

struct Transcript {
    id: String,
    title: String,
    tool_call: String,
    path: PathBuf,
    status: &'static str,
}
impl Transcript {
    fn upsert(
        &self,
        provider: &str,
        status: &str,
        title: &str,
        timestamp: Option<String>,
    ) -> Value {
        let mut event = json!({"type":"upsert","id":self.id,"title":title,"status":status,
            "toolCallId":self.tool_call,
            "source":{"path":self.path.to_string_lossy(),"title":self.title}});
        if let Some(timestamp) = timestamp {
            event["timestamp"] = json!(timestamp);
        }
        json!({"type":"provider_subagent","provider":provider,"event":event})
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn subagent_transcripts(messages: &[Value], parent: &Path) -> Vec<Transcript> {
    let mut calls = HashMap::new();
    let mut out: Vec<Transcript> = Vec::new();
    let mut children: HashMap<Cow<'_, str>, usize> = HashMap::new();
    for message in messages {
        if message["role"] == "assistant" {
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "toolCall" && block["name"] == "task"
                    && let Some(id) = block["id"].as_str() {
                        calls.insert(
                            id,
                            block["arguments"]["agent"]
                                .as_str()
                                .unwrap_or("OMP subagent"),
                        );
                    }
            }
        }
        for (id, status) in omp_task_completions(message) {
            if let Some(index) = children.get(id)
                && out[*index].status == "running" {
                    out[*index].status = status;
                }
        }
        if message["role"] != "toolResult" || message["toolName"] != "task" {
            continue;
        }
        let id = message["toolCallId"].as_str().unwrap_or("");
        let Some(title) = calls.get(id) else { continue };
        let details = message
            .get("details")
            .filter(|v| !v.is_null())
            .unwrap_or(&message["content"]["details"]);
        let mut found = false;
        // Results take precedence when both arrays describe the same child.
        for (rows, terminal) in [(&details["results"], true), (&details["progress"], false)] {
            for result in rows.as_array().into_iter().flatten() {
                let Some(child_id) = result["id"].as_str().filter(|s| !s.is_empty()) else {
                    continue;
                };
                found = true;
                let existing = children.get(child_id).copied();
                if existing.is_some_and(|index| out[index].tool_call == id) {
                    continue;
                }
                let status = if terminal && (message["isError"] == true || truthy(&result["error"])) {
                    "failed"
                } else {
                    omp_task_status(result, terminal)
                };
                let basename = Path::new(child_id)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let path = result["sessionFile"].as_str().filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| parent.with_extension("").join(format!("{basename}.jsonl")));
                let transcript = Transcript {
                    id: child_id.to_owned(),
                    title: result["agent"].as_str().unwrap_or(title).to_owned(),
                    tool_call: id.to_owned(),
                    path,
                    status,
                };
                if let Some(index) = existing {
                    out[index] = transcript;
                } else {
                    children.insert(Cow::Borrowed(child_id), out.len());
                    out.push(transcript);
                }
            }
        }
        if found {
            continue;
        }
        let text = message["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if let Some(capture) = LEGACY_TRANSCRIPT.captures(&text) {
            let path = PathBuf::from(&capture[1]);
            let child_id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            if children.contains_key(child_id.as_str()) {
                continue;
            }
            children.insert(Cow::Owned(child_id.clone()), out.len());
            out.push(Transcript {
                id: child_id,
                title: (*title).to_owned(),
                tool_call: id.to_owned(),
                path,
                status: if message["isError"] == true {
                    "failed"
                } else {
                    "completed"
                },
            });
        }
    }
    out
}

fn subagent_title(title: &str, model: Option<&str>) -> String {
    let title = if title.trim().is_empty() {
        "OMP subagent"
    } else {
        title.trim()
    };
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        None => title.to_owned(),
        Some(model) => match model
            .split_once('/')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        {
            Some((provider, name)) => format!("{title} · {name} ({provider})"),
            None => format!("{title} · {model}"),
        },
    }
}

pub fn todo_state(entries: &[Value]) -> Option<Value> {
    let message = entries
        .iter()
        .rev()
        .find(|e| e["message"]["role"] == "toolResult" && e["message"]["toolName"] == "todo")?
        .get("message")?;
    let phases = message["details"]["phases"].as_array()?;
    let mut tasks = Vec::new();
    for phase in phases {
        phase["name"].as_str()?;
        for task in phase["tasks"].as_array()? {
            let content = task["content"].as_str()?;
            let status = task["status"].as_str()?;
            if !matches!(
                status,
                "pending" | "in_progress" | "completed" | "blocked" | "abandoned"
            ) {
                return None;
            }
            if task.get("blocker").is_some_and(|v| !v.is_string()) {
                return None;
            }
            tasks.push((content, status, task["blocker"].as_str()));
        }
    }
    if tasks.is_empty() {
        return None;
    }
    Some(
        json!({"type":"todo","items":tasks.into_iter().filter(|(_,status,_)| *status != "abandoned").map(|(content,status,blocker)| {
        let text = if status == "blocked" { format!("{content} ({})", blocker.filter(|s| !s.is_empty()).map(|s| format!("blocked: {s}")).unwrap_or_else(|| "blocked".into())) } else { content.to_owned() };
        json!({"text":text,"status":if matches!(status,"in_progress" | "completed") {status} else {"pending"},"completed":status == "completed"})
    }).collect::<Vec<_>>()}),
    )
}

pub(super) fn limit_child_item(mut item: Value) -> Value {
    if item["type"] != "tool_call" {
        return item;
    }
    let shell = item["detail"]["type"] == "shell";
    let plain = item["detail"]["type"] == "plain_text";
    let failed = item["status"] == "failed";
    for (pointer, enabled, notice) in [
        ("/error/content", shell && failed, ""),
        ("/detail/output", shell, ""),
        ("/detail/text", plain, "\n\n[Content truncated]"),
    ] {
        if !enabled {
            continue;
        }
        let Some(value) = item.pointer_mut(pointer) else {
            continue;
        };
        let Some(text) = value.as_str() else { continue };
        if text.encode_utf16().count() <= 64 * 1024 {
            continue;
        }
        let prefix: Vec<_> = text.encode_utf16().take(64 * 1024 - notice.len()).collect();
        *value = json!(format!("{}{notice}", String::from_utf16_lossy(&prefix)));
    }
    item
}

fn equivalent_paths(left: &str, right: &str) -> bool {
    let windows = WINDOWS_PATH.is_match(left) || WINDOWS_PATH.is_match(right);
    normalize_comparison_path(left, windows) == normalize_comparison_path(right, windows)
}

fn normalize_comparison_path(value: &str, windows: bool) -> String {
    let mut value = if windows {
        value.replace('\\', "/").to_lowercase()
    } else {
        value.to_owned()
    };
    if windows && value.starts_with("//?/unc/") {
        value = format!("//{}", &value[8..]);
    } else if windows && value.starts_with("//?/") {
        value.drain(..4);
    }
    let (root, rest) = if windows && value.as_bytes().get(1) == Some(&b':') {
        if value.as_bytes().get(2) == Some(&b'/') {
            (value[..3].to_owned(), &value[3..])
        } else {
            (value[..2].to_owned(), &value[2..])
        }
    } else if windows && value.starts_with("//") {
        let mut split = value[2..].splitn(3, '/');
        let server = split.next().unwrap_or("");
        let share = split.next().unwrap_or("");
        (format!("//{server}/{share}/"), split.next().unwrap_or(""))
    } else if let Some(rest) = value.strip_prefix('/') {
        ("/".to_owned(), rest)
    } else {
        (String::new(), value.as_str())
    };
    let absolute = root.ends_with('/');
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        if root.is_empty() { ".".into() } else { root }
    } else {
        format!("{root}{}", parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        json!([{"type":"text","text":value}])
    }

    /// A Pi file after `paseo_tree` rewound to u2: navigateTree moved the leaf
    /// to a1, then the extension appended the paseo_rewind marker there.
    fn rewound_session() -> String {
        [
            json!({"type":"session","id":"session","cwd":"/work","timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":text("first")}}),
            json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":text("one")}}),
            json!({"type":"message","id":"u2","parentId":"a1","message":{"role":"user","content":text("second")}}),
            json!({"type":"message","id":"a2","parentId":"u2","message":{"role":"assistant","content":text("two")}}),
            json!({"type":"custom","id":"r1","parentId":"a1","customType":"paseo_rewind","data":{"targetId":"u2"}}),
        ]
        .iter()
        .map(|entry| format!("{entry}\n"))
        .collect()
    }

    #[tokio::test]
    async fn pi_file_ending_at_live_leaf_replays_rewound_branch_completely() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(&path, rewound_session()).await.unwrap();
        let history = verified_pi_live("pi", &path, &json!("r1")).await.unwrap()
            .expect("a complete file ending at the live leaf is authoritative");
        let texts: Vec<_> = history.timeline.iter().filter_map(|item| item["text"].as_str()).collect();
        assert_eq!(texts, ["first", "one"]);
        assert_eq!(history.timeline[0]["messageId"], "u1");
    }

    #[tokio::test]
    async fn pi_file_not_proven_at_live_leaf_is_not_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        // Unflushed or older Pi: no file, or no live leaf evidence.
        assert!(verified_pi_live("pi", &path, &json!("r1")).await.unwrap().is_none());
        fs::write(&path, rewound_session()).await.unwrap();
        // The live runtime is behind or ahead of the file, or reported no leaf.
        for leaf in [json!("a2"), json!("a1"), json!("later"), Value::Null] {
            assert!(verified_pi_live("pi", &path, &leaf).await.unwrap().is_none(), "{leaf}");
        }
        // A torn append can hide entries even when the last valid entry matches.
        fs::write(&path, rewound_session() + "{\"type\":\"message\",\"id\":\"u3\"").await.unwrap();
        assert!(verified_pi_live("pi", &path, &json!("r1")).await.unwrap().is_none());
    }

    fn lines(entries: &[Value]) -> String {
        entries.iter().map(|entry| format!("{entry}\n")).collect()
    }

    /// An OMP session whose `task` tool spawned `children`, each with an agent name.
    fn omp_session(id: &str, children: &[(&str, &str)], reply: &str) -> String {
        let results: Vec<_> =
            children.iter().map(|(child, agent)| json!({"id":child,"agent":agent})).collect();
        lines(&[
            json!({"type":"session","id":id,"cwd":"/work","timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"model_change","id":"m0","parentId":null,"provider":"anthropic","modelId":"claude"}),
            json!({"type":"message","id":"u1","parentId":"m0","timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"role":"user","content":text("delegate")}}),
            json!({"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z",
                "message":{"role":"assistant","content":[{"type":"text","text":reply},
                    {"type":"toolCall","id":"task-1","name":"task","arguments":{"agent":"worker"}}]}}),
            json!({"type":"message","id":"r1","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z",
                "message":{"role":"toolResult","toolCallId":"task-1","toolName":"task",
                    "content":text("done"),"details":{"results":results}}}),
        ])
    }

    fn omp_background_session(id: &str, progress: Value, jobs: Value) -> String {
        lines(&[
            json!({"type":"session","id":id,"cwd":"/work","timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"message","id":"a1","parentId":null,"message":{"role":"assistant",
                "content":[{"type":"text","text":"child body"},{"type":"toolCall","id":"task-1","name":"task",
                    "arguments":{"tasks":[{"name":"worker","agent":"scout"}]}}]}}),
            json!({"type":"message","id":"r1","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z",
                "message":{"role":"toolResult","toolCallId":"task-1","toolName":"task",
                    "content":text("queued"),"details":{"async":true,"results":[],"progress":progress}}}),
            json!({"type":"custom_message","id":"c1","parentId":"r1","customType":"async-result",
                "timestamp":"2026-01-01T00:00:04.000Z","content":"<system-notice>Background delivery</system-notice>",
                "details":{"jobs":jobs}}),
        ])
    }

    #[tokio::test]
    async fn omp_progress_history_preserves_children_and_async_outcomes() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        let explicit = directory.path().join("explicit.jsonl");
        fs::write(&parent, omp_background_session("parent", json!([
            {"id":"Exact.Child","agent":"scout","status":"pending","index":0},
            {"id":"failed-child","agent":"reviewer","status":"running","index":1,"sessionFile":explicit},
            {"id":"still-running","agent":"worker","status":"queued","index":2},
            {"id":"aborted-child","agent":"worker","status":"pending","index":3}
        ]), json!([
            {"type":"task","jobId":"Exact.Child","agentId":"Exact.Child","status":"completed"},
            {"type":"task","jobId":"failed-child","agentId":"failed-child","status":"failed"},
            {"type":"task","jobId":"aborted-child","agentId":"aborted-child","status":"canceled"},
            {"type":"bash","jobId":"still-running","status":"completed"},
            {"type":"task","jobId":"unknown","status":"completed"}
        ]))).await.unwrap();
        // Directories at every locator prove descriptor recovery reads no child body.
        for path in [
            parent.with_extension("").join("Exact.Child.jsonl"),
            explicit.clone(),
            parent.with_extension("").join("still-running.jsonl"),
            parent.with_extension("").join("aborted-child.jsonl"),
        ] {
            fs::create_dir_all(path).await.unwrap();
        }
        let history = load_live("omp", "custom-omp", &parent, None, &Value::Null).await.unwrap();
        let children: Vec<_> = history.subagent_events.iter().map(|e| &e["event"]).collect();
        assert_eq!(children.len(), 4);
        for (child, (id, title, status)) in children.iter().zip([
            ("Exact.Child", "scout", "completed"),
            ("failed-child", "reviewer", "failed"),
            ("still-running", "worker", "running"),
            ("aborted-child", "worker", "canceled"),
        ]) {
            assert_eq!(child["id"], id);
            assert_eq!(child["title"], title);
            assert_eq!(child["status"], status);
            assert_eq!(child["toolCallId"], "task-1");
            assert_eq!(child["type"], "upsert");
        }
        assert_eq!(children[0]["source"]["path"], json!(parent.with_extension("").join("Exact.Child.jsonl")));
        assert_eq!(children[1]["source"]["path"], json!(explicit));
    }

    #[tokio::test]
    async fn omp_progress_child_without_source_replays_nested_outcomes() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        let child = parent.with_extension("").join("child.jsonl");
        fs::create_dir_all(child.parent().unwrap()).await.unwrap();
        fs::write(&parent, omp_background_session("parent", json!([
            {"id":"child","agent":"scout","status":"pending"},
            {"id":"sibling","agent":"worker","status":"pending"}
        ]), json!([]))).await.unwrap();
        fs::create_dir_all(parent.with_extension("").join("sibling.jsonl")).await.unwrap();
        fs::write(&child, omp_background_session("child", json!([
            {"id":"nested-ok","agent":"scout","status":"pending"},
            {"id":"nested-failed","agent":"worker","status":"pending"}
        ]), json!([
            {"type":"task","agentId":"nested-ok","status":"completed"},
            {"type":"task","agentId":"nested-failed","status":"failed"}
        ]))).await.unwrap();
        let events = child_history("omp", "omp", &request("child", &parent, Value::Null)).await.unwrap();
        assert_eq!(events[0]["event"]["title"], "scout");
        let rows: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "timeline").collect();
        assert!(rows.iter().any(|e| e["event"]["item"]["text"] == "child body"));
        assert!(rows.iter().all(|e| e["event"]["id"] == "child" && e["event"]["historyComplete"] == true));
        let nested: Vec<_> = events.iter().filter(|e| e["event"]["parentSubagentId"] == "child").collect();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0]["event"]["id"], "nested-ok");
        assert_eq!(nested[0]["event"]["status"], "completed");
        assert_eq!(nested[1]["event"]["id"], "nested-failed");
        assert_eq!(nested[1]["event"]["status"], "failed");
        assert_eq!(nested[1]["event"]["source"]["path"], json!(child.with_extension("").join("nested-failed.jsonl")));
    }

    #[test]
    fn omp_task_history_rejects_text_claims_and_unrelated_identities() {
        let call = |id: &str| json!({"role":"assistant","content":[
            {"type":"toolCall","name":"task","id":id,"arguments":{"agent":"worker"}}]});
        let result = |call: &str, child: &str, agent: &str| json!({"role":"toolResult","toolName":"task",
            "toolCallId":call,"details":{"results":[],"progress":[{"id":child,"agent":agent,"status":"pending"}]}});
        let messages = [
            call("task-1"),
            result("task-1", "known", "scout"),
            json!({"role":"user","customType":"async-result",
                "content":"<task-result id=\"known\" status=\"failed\">",
                "details":{"jobs":[{"type":"task","agentId":"known","status":"failed"}],
                    "progress":[{"id":"invented","status":"completed"}]}}),
            json!({"role":"custom","customType":"async-result",
                "content":"<task-result id=\"known\" status=\"failed\">",
                "details":{"jobs":[{"type":"task","agentId":"unrelated","status":"completed"}]}}),
            result("unknown-call", "invented", "wrong"),
        ];
        let children = subagent_transcripts(&messages, Path::new("/tmp/parent.jsonl"));
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "known");
        assert_eq!(children[0].title, "scout");
        assert_eq!(children[0].tool_call, "task-1");
        assert_eq!(children[0].status, "running");
    }

    #[test]
    fn omp_task_history_does_not_apply_earlier_completion_to_later_spawn() {
        let call = |id: &str| json!({"role":"assistant","content":[
            {"type":"toolCall","name":"task","id":id,"arguments":{"agent":"worker"}}]});
        let result = |id: &str| json!({"role":"toolResult","toolName":"task","toolCallId":id,
            "details":{"results":[],"progress":[{"id":"reused","status":"pending"}]}});
        let delivery = json!({"role":"custom","customType":"async-result",
            "details":{"jobs":[{"type":"task","agentId":"reused","status":"completed"}]}});
        let messages = [
            delivery.clone(),
            call("first"),
            result("first"),
            delivery,
            call("second"),
            result("second"),
        ];
        let children = subagent_transcripts(&messages, Path::new("/tmp/parent.jsonl"));
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].status, "running");
        assert_eq!(children[0].tool_call, "second");
    }

    #[test]
    fn omp_legacy_transcript_locator_and_terminal_results_remain_supported() {
        let messages = [
            json!({"role":"assistant","content":[
                {"type":"toolCall","name":"task","id":"legacy","arguments":{"agent":"reviewer"}},
                {"type":"toolCall","name":"task","id":"modern","arguments":{"agent":"worker"}}]}),
            json!({"role":"toolResult","toolName":"task","toolCallId":"legacy",
                "content":text("Session file: /tmp/legacy-child.jsonl")}),
            json!({"role":"toolResult","toolName":"task","toolCallId":"modern","details":{"results":[
                {"id":"done"},{"id":"bad","exitCode":1},{"id":"stopped","aborted":true}],
                "progress":[{"id":"done","status":"pending"}]}}),
        ];
        let children = subagent_transcripts(&messages, Path::new("/tmp/parent.jsonl"));
        assert_eq!(children.len(), 4);
        assert_eq!(children[0].path, Path::new("/tmp/legacy-child.jsonl"));
        assert_eq!(children[0].title, "reviewer");
        assert_eq!(children.iter().map(|c| c.status).collect::<Vec<_>>(), ["completed", "completed", "failed", "canceled"]);
    }

    fn request(child: &str, parent: &Path, source: Value) -> SubagentHistoryRequest {
        SubagentHistoryRequest {
            persistence: json!({"sessionId":"parent","nativeHandle":parent}),
            config: json!({"cwd":"/work"}),
            child_id: child.into(),
            source,
            status: json!("completed"),
            replay: crate::providers::api::SubagentReplay::new(
                "replay".into(),
                tokio::sync::watch::channel(None).1,
            ),
        }
    }

    #[tokio::test]
    async fn omp_parent_history_announces_children_without_reading_their_transcripts() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        fs::write(&parent, omp_session("parent", &[("child-a", "reviewer"), ("child-b", "scout")], "parent text"))
            .await.unwrap();
        let children = directory.path().join("parent");
        // An unreadable sibling fails any read of it: parent recovery must not touch it.
        fs::create_dir_all(children.join("child-a.jsonl")).await.unwrap();
        fs::create_dir_all(children.join("child-b.jsonl")).await.unwrap();
        let history = load_live("omp", "omp", &parent, None, &Value::Null).await.unwrap();
        assert!(history.complete);
        assert!(history.timeline.iter().any(|item| item["text"] == "parent text"));
        let descriptors: Vec<_> = history.subagent_events.iter().map(|e| &e["event"]).collect();
        assert!(descriptors.iter().all(|e| e["type"] == "upsert"), "{descriptors:?}");
        let ids: Vec<_> = descriptors.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["child-a", "child-b"]);
        assert_eq!(descriptors[0]["title"], "reviewer");
        assert_eq!(descriptors[0]["status"], "completed");
        assert_eq!(descriptors[0]["toolCallId"], "task-1");
        assert_eq!(descriptors[0]["timestamp"], "2026-01-01T00:00:03.000Z");
        assert_eq!(descriptors[0]["source"]["path"], json!(children.join("child-a.jsonl")));
    }

    #[tokio::test]
    async fn opened_omp_child_replays_only_its_transcript_and_announces_grandchildren() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        fs::write(&parent, omp_session("parent", &[("child-a", "reviewer"), ("child-b", "scout")], "parent text"))
            .await.unwrap();
        let children = directory.path().join("parent");
        fs::create_dir_all(children.join("child-b.jsonl")).await.unwrap();
        fs::write(children.join("child-a.jsonl"), omp_session("child-a", &[("grand", "digger")], "child text"))
            .await.unwrap();
        let history = load_live("omp", "omp", &parent, None, &Value::Null).await.unwrap();
        let source = history.subagent_events[0]["event"]["source"].clone();
        for source in [source, Value::Null] {
            let events = child_history("omp", "omp", &request("child-a", &parent, source)).await.unwrap();
            assert_eq!(events[0]["event"], json!({"type":"upsert","id":"child-a","title":"reviewer · claude (anthropic)"}));
            let rows: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "timeline").collect();
            assert!(rows.iter().any(|row| row["event"]["item"]["text"] == "child text"));
            assert!(!rows.iter().any(|row| row["event"]["item"]["text"] == "parent text"));
            assert!(rows.iter().all(|row| row["event"]["id"] == "child-a" && row["event"]["historyComplete"] == true));
            let nested: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "upsert").skip(1).collect();
            assert_eq!(nested.len(), 1);
            assert_eq!(nested[0]["event"]["id"], "grand");
            assert_eq!(nested[0]["event"]["parentSubagentId"], "child-a");
            assert_eq!(nested[0]["event"]["source"]["path"], json!(children.join("child-a").join("grand.jsonl")));
        }
        let sibling = child_history("omp", "omp", &request("child-b", &parent, Value::Null)).await;
        assert!(sibling.unwrap_err().to_string().contains("unavailable"));
        fs::remove_file(children.join("child-a.jsonl")).await.unwrap();
        let missing = child_history("omp", "omp", &request("child-a", &parent, Value::Null)).await;
        assert!(missing.unwrap_err().to_string().contains("unavailable"), "a missing body is never an empty replay");
        let unknown = child_history("omp", "omp", &request("stranger", &parent, Value::Null)).await;
        assert!(unknown.is_err());
    }

    #[tokio::test]
    async fn pi_parent_attaches_child_file_and_opened_child_reads_it() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        let child = directory.path().join("reviewer.jsonl");
        let output = format!("Output file: {}", child.display());
        fs::write(&parent, lines(&[
            json!({"type":"session","id":"parent","cwd":"/work","timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":text("delegate")}}),
            json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[
                {"type":"toolCall","id":"spawn","name":"subagent","arguments":{"subagent_type":"reviewer","prompt":"Review"}}]}}),
            json!({"type":"message","id":"r1","parentId":"a1","message":{"role":"toolResult","toolCallId":"spawn",
                "toolName":"subagent","content":text(&output),"details":{"agentId":"native-child","status":"completed"}}}),
        ])).await.unwrap();
        let body = lines(&[
            json!({"type":"message","timestamp":"2026-01-01T00:00:05.000Z","message":{"role":"assistant","content":text("child text")}}),
        ]);
        fs::write(&child, &body).await.unwrap();
        let history = load_live("pi", "pi", &parent, None, &Value::Null).await.unwrap();
        assert!(history.subagent_events.iter().all(|e| e["event"]["type"] != "timeline"),
            "parent recovery never replays the child file");
        let source = history.subagent_events.iter().map(|e| &e["event"])
            .find(|e| e["id"] == "spawn" && e["source"].is_object()).expect("child file attached")["source"].clone();
        assert_eq!(source, json!({"file":child}));
        for source in [source, Value::Null] {
            let events = child_history("pi", "pi", &request("spawn", &parent, source)).await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["event"]["item"]["text"], "child text");
            assert_eq!(events[0]["event"]["historyComplete"], true);
            assert_eq!(events[0]["event"]["timestamp"], "2026-01-01T00:00:05.000Z");
        }
        fs::write(&child, body + "{\"type\":\"message\"").await.unwrap();
        let torn = child_history("pi", "pi", &request("spawn", &parent, json!({"file":child}))).await.unwrap();
        assert_eq!(torn[0]["event"]["historyComplete"], false, "a torn tail is not complete");
        fs::remove_file(&child).await.unwrap();
        let missing = child_history("pi", "pi", &request("spawn", &parent, json!({"file":child}))).await;
        assert!(missing.unwrap_err().to_string().contains("unavailable"));
    }

    #[tokio::test]
    async fn omp_live_child_rows_and_file_replay_share_native_ids() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent.jsonl");
        let path = directory.path().join("parent").join("child-a.jsonl");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let message = |text_value: &str, stamp: u64| {
            json!({"role":"assistant","content":text(text_value),"timestamp":stamp})
        };
        fs::write(&path, lines(&[
            json!({"type":"session","id":"child-a","cwd":"/work","timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"message","id":"m1","parentId":null,"message":message("earlier", 1_000)}),
            json!({"type":"message","id":"m2","parentId":"m1","message":message("latest", 2_000)}),
        ])).await.unwrap();
        // The live stream began after the child's first message: its mapper never saw it.
        let mut live = Mapper::new("omp", "omp");
        let live_rows = live.map(&json!({"type":"subagent_event","payload":{"id":"child-a",
            "event":{"type":"message_end","message":message("latest", 2_000)}}}));
        let live_row = live_rows
            .iter()
            .find(|e| e["type"] == "provider_subagent" && e["event"]["type"] == "timeline")
            .expect("live child row");
        let replay = child_history(
            "omp",
            "omp",
            &request("child-a", &parent, json!({"path":path,"title":"reviewer"})),
        )
        .await
        .unwrap();
        let rows: Vec<_> = replay.iter().filter(|e| e["event"]["type"] == "timeline").collect();
        let ids: Vec<_> = rows.iter().map(|e| e["event"]["item"]["messageId"].clone()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(rows[1]["event"]["item"], live_row["event"]["item"], "the same record, read live or replayed");
    }
}
