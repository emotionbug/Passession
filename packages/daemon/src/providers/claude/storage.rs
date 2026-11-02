use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
};
use uuid::Uuid;

pub struct ReplaySubagent {
    pub agent_id: String,
    pub meta: Option<Value>,
    pub entries: Vec<Value>,
    /// The sidechain file holding this agent's body, read only when the agent is
    /// opened. None: the body was inline in the transcript already read.
    pub file: Option<PathBuf>,
}

#[derive(Default)]
pub struct ReplayHistory {
    pub parent_entries: Vec<Value>,
    pub subagents: Vec<ReplaySubagent>,
    /// Workflow run summaries; their entry files are read only when opened.
    pub workflows: Vec<Value>,
    pub complete: bool,
}

/// Sidechain files and agent metadata beside one transcript, without their bodies.
#[derive(Default)]
struct SidechainIndex {
    /// `agent-<agentId>.jsonl` files by agent ID.
    files: Vec<(String, PathBuf)>,
    /// Sidechain files whose name does not identify their agent.
    unnamed: Vec<PathBuf>,
    workflow_files: BTreeMap<String, Vec<PathBuf>>,
    meta: HashMap<String, Value>,
}

pub fn config_dir(env: &BTreeMap<String, String>) -> Result<PathBuf> {
    if let Some(path) = env.get("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("Cannot resolve Claude home directory"))?
        .join(".claude"))
}

/// The SDK regex has no Unicode flag: each UTF-16 code unit becomes one dash.
pub fn encode_project_path(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len().min(200) + 8);
    let mut count = 0usize;
    let mut hash = 0i32;
    for unit in input.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
        if count < 200 {
            encoded.push(if unit < 128 && (unit as u8).is_ascii_alphanumeric() {
                unit as u8 as char
            } else {
                '-'
            });
        }
        count += 1;
    }
    if count > 200 {
        encoded.push('-');
        let mut value = i64::from(hash).unsigned_abs();
        let mut digits = [0u8; 7];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize];
            value /= 36;
            if value == 0 {
                break;
            }
        }
        for digit in &digits[start..] {
            encoded.push(*digit as char);
        }
    }
    encoded
}

pub async fn project_dir(cwd: &str, env: &BTreeMap<String, String>) -> Result<PathBuf> {
    let canonical = fs::canonicalize(cwd)
        .await
        .unwrap_or_else(|_| PathBuf::from(cwd));
    let canonical = canonical.to_string_lossy();
    #[cfg(target_os = "macos")]
    let canonical = {
        use unicode_normalization::UnicodeNormalization;
        canonical.nfc().collect::<String>()
    };
    Ok(config_dir(env)?
        .join("projects")
        .join(encode_project_path(&canonical)))
}

pub async fn transcript_path(
    cwd: &str,
    session_id: &str,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    let expected = project_dir(cwd, env).await?.join(&file_name);
    if fs::try_exists(&expected).await.unwrap_or(false) {
        return Ok(expected);
    }
    let projects = config_dir(env)?.join("projects");
    let entries = match directory_entries(&projects).await {
        Ok(entries) => entries,
        Err(error) if missing(&error) => return Ok(expected),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let candidate = entry.path().join(&file_name);
        if fs::try_exists(&candidate).await.unwrap_or(false) {
            return Ok(candidate);
        }
    }
    Ok(expected)
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn expected_import_error(error: &io::Error) -> bool {
    missing(error) || error.kind() == io::ErrorKind::PermissionDenied
}

async fn directory_entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut directory = fs::read_dir(path).await?;
    let mut entries = Vec::new();
    while let Some(entry) = directory.next_entry().await? {
        entries.push(entry);
    }
    // Node readdir returns name order; traversal and equal-timestamp records retain it.
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

async fn visit_records(path: &Path, mut visit: impl FnMut(Value)) -> Result<bool> {
    let file = fs::File::open(path)
        .await
        .with_context(|| format!("Cannot read Claude transcript {}", path.display()))?;
    let metadata = file.metadata().await?;
    if metadata.is_dir() {
        bail!("Claude transcript is a directory: {}", path.display());
    }
    // Read the opened inode's initial extent, not newly appended messages.
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(metadata.len()));
    let mut line = Vec::new();
    let mut complete = true;
    while reader.read_until(b'\n', &mut line).await? != 0 {
        if let Ok(entry @ Value::Object(_)) = serde_json::from_slice::<Value>(&line) {
            visit(entry);
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            complete = false;
        }
        line.clear();
    }
    Ok(complete)
}

async fn optional_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).await.ok()?).ok()
}

fn filtered_fields(value: &Value, strings: &[&str], numbers: &[&str]) -> Option<Value> {
    value.as_object()?;
    let mut result = serde_json::Map::new();
    for key in strings {
        if let Some(value @ Value::String(_)) = value.get(*key) {
            result.insert((*key).into(), value.clone());
        }
    }
    for key in numbers {
        if let Some(value @ Value::Number(_)) = value.get(*key) {
            result.insert((*key).into(), value.clone());
        }
    }
    Some(Value::Object(result))
}

async fn sidechain_index(path: &Path) -> Result<SidechainIndex> {
    let sidechain_directory = path.with_extension("").join("subagents");
    let mut index = SidechainIndex::default();
    let mut directories = vec![sidechain_directory.clone()];
    while let Some(directory) = directories.pop() {
        let entries = match directory_entries(&directory).await {
            Ok(entries) => entries,
            Err(error) if missing(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                directories.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let file = entry.path();
            if name.ends_with(".jsonl") {
                let parts: Vec<_> = file
                    .strip_prefix(&sidechain_directory)?
                    .components()
                    .collect();
                if parts.len() >= 3 && parts[0].as_os_str() == "workflows" {
                    index
                        .workflow_files
                        .entry(parts[1].as_os_str().to_string_lossy().into_owned())
                        .or_default()
                        .push(file);
                } else if let Some(agent_id) = name
                    .strip_prefix("agent-")
                    .and_then(|name| name.strip_suffix(".jsonl"))
                    .filter(|id| !id.is_empty())
                {
                    index.files.push((agent_id.to_owned(), file));
                } else {
                    index.unnamed.push(file);
                }
            } else if let Some(agent_id) = name
                .strip_prefix("agent-")
                .and_then(|name| name.strip_suffix(".meta.json"))
                .filter(|id| !id.is_empty())
            {
                if let Some(value) = optional_json(&file).await.and_then(|value| {
                    filtered_fields(
                        &value,
                        &["agentType", "description", "toolUseId"],
                        &["spawnDepth"],
                    )
                }) {
                    if value.as_object().is_some_and(|object| !object.is_empty()) {
                        index.meta.insert(agent_id.to_owned(), value);
                    }
                }
            }
        }
    }
    Ok(index)
}

/// Groups `isSidechain` records of `file` by agent, in first-seen order.
async fn visit_sidechains(
    file: &Path,
    subagents: &mut Vec<ReplaySubagent>,
    meta: &mut HashMap<String, Value>,
    mut parent: Option<&mut Vec<Value>>,
) -> Result<bool> {
    visit_records(file, |entry| {
        if entry.get("isSidechain") != Some(&Value::Bool(true)) {
            if let Some(parent) = parent.as_mut() {
                parent.push(entry);
            }
            return;
        }
        let Some(agent_id) = entry.get("agentId").and_then(Value::as_str) else {
            return;
        };
        let index = match subagents.iter().position(|child| child.agent_id == agent_id) {
            Some(index) => index,
            None => {
                subagents.push(ReplaySubagent {
                    agent_id: agent_id.to_owned(),
                    meta: meta.remove(agent_id),
                    entries: Vec::new(),
                    file: None,
                });
                subagents.len() - 1
            }
        };
        subagents[index].entries.push(entry);
    })
    .await
}

/// The parent transcript with sidechains already inline in it. File-backed agents
/// are announced from their file name and metadata; their bodies stay unread.
pub async fn read_history(path: &Path) -> Result<ReplayHistory> {
    let mut index = sidechain_index(path).await?;
    let mut history = ReplayHistory::default();
    history.complete = visit_sidechains(
        path,
        &mut history.subagents,
        &mut index.meta,
        Some(&mut history.parent_entries),
    )
    .await?;
    for file in &index.unnamed {
        visit_sidechains(file, &mut history.subagents, &mut index.meta, None).await?;
    }
    for (agent_id, file) in index.files {
        if history.subagents.iter().any(|child| child.agent_id == agent_id) {
            // A body split between the transcript and its own file stays whole.
            visit_sidechains(&file, &mut history.subagents, &mut index.meta, None).await?;
        } else {
            history.subagents.push(ReplaySubagent {
                meta: index.meta.remove(&agent_id),
                agent_id,
                entries: Vec::new(),
                file: Some(file),
            });
        }
    }
    history.workflows = read_workflows(path).await?;
    Ok(history)
}

/// One opened agent's sidechain transcript, and every other file-backed agent's
/// descriptor so the children it spawned can be linked without reading them.
pub async fn read_subagent(
    path: &Path,
    agent_id: &str,
) -> Result<(ReplaySubagent, Vec<ReplaySubagent>, bool)> {
    let mut index = sidechain_index(path).await?;
    let file = index
        .files
        .iter()
        .find(|(id, _)| id == agent_id)
        .map(|(_, file)| file.clone())
        .ok_or_else(|| anyhow!("Claude subagent {agent_id} has no sidechain transcript"))?;
    let mut entries = Vec::new();
    let complete = visit_records(&file, |entry| {
        if entry.get("isSidechain") == Some(&Value::Bool(true))
            && entry.get("agentId").and_then(Value::as_str) == Some(agent_id)
        {
            entries.push(entry);
        }
    })
    .await?;
    let child = ReplaySubagent {
        agent_id: agent_id.to_owned(),
        meta: index.meta.remove(agent_id),
        entries,
        file: Some(file),
    };
    let others = index
        .files
        .into_iter()
        .filter(|(id, _)| id != agent_id)
        .map(|(id, file)| ReplaySubagent {
            meta: index.meta.remove(&id),
            agent_id: id,
            entries: Vec::new(),
            file: Some(file),
        })
        .collect();
    Ok((child, others, complete))
}

/// Keeps the assistant records of messages the transcript records as finished:
/// some record of the message carries the API's `stop_reason`, which is set only
/// once the message stopped. Records of any other assistant message are dropped,
/// since their text may still be growing. Returns the finished message IDs and
/// whether anything was dropped.
pub fn keep_finished_messages(entries: &mut Vec<Value>) -> (HashSet<String>, bool) {
    let finished: HashSet<String> = entries
        .iter()
        .filter(|entry| entry["type"] == "assistant" && !entry["message"]["stop_reason"].is_null())
        .filter_map(|entry| entry["message"]["id"].as_str().map(str::to_owned))
        .collect();
    let before = entries.len();
    entries.retain(|entry| {
        entry["type"] != "assistant"
            || entry["message"]["id"].as_str().is_some_and(|id| finished.contains(id))
    });
    let dropped = entries.len() != before;
    (finished, dropped)
}

/// One opened workflow run: its summary, its entries in timestamp order, and
/// whether every entry file was read without a torn record.
pub async fn read_workflow(path: &Path, run_id: &str) -> Result<(Value, Vec<Value>, bool)> {
    let workflow = read_workflows(path)
        .await?
        .into_iter()
        .find(|workflow| workflow["runId"] == run_id)
        .ok_or_else(|| anyhow!("Claude workflow {run_id} has no run record"))?;
    let mut entries = Vec::new();
    let mut complete = true;
    let files = sidechain_index(path)
        .await?
        .workflow_files
        .remove(run_id)
        .unwrap_or_default();
    for file in files {
        complete &= visit_records(&file, |entry| {
            if entry.get("type").and_then(Value::as_str) != Some("user")
                || has_tool_result(&entry)
            {
                entries.push(entry);
            }
        })
        .await?;
    }
    entries.sort_by_key(|entry| (timestamp(entry).is_none(), timestamp(entry)));
    Ok((workflow, entries, complete))
}

async fn read_workflows(path: &Path) -> Result<Vec<Value>> {
    let workflow_directory = path.with_extension("").join("workflows");
    let entries = match directory_entries(&workflow_directory).await {
        Ok(entries) => entries,
        Err(error) if missing(&error) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut workflows = Vec::new();
    for entry in entries {
        if !entry.file_type().await?.is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("json")
        {
            continue;
        }
        if let Some(raw) = optional_json(&entry.path()).await {
            if raw.get("runId").and_then(Value::as_str).is_none() {
                continue;
            }
            if let Some(mut workflow) = filtered_fields(
                &raw,
                &[
                    "runId",
                    "timestamp",
                    "summary",
                    "workflowName",
                    "status",
                    "defaultModel",
                ],
                &["startTime", "totalTokens"],
            ) {
                if let Some(result) = raw.get("result") {
                    workflow["result"] = result.clone();
                }
                workflows.push(workflow);
            }
        }
    }
    Ok(workflows)
}

fn timestamp(value: &Value) -> Option<i64> {
    let value = value.get("timestamp")?;
    if let Some(number) = value.as_f64() {
        let millis = if number > 1_000_000_000_000.0 {
            number
        } else {
            number * 1000.0
        };
        return (millis.is_finite() && millis.abs() <= 8_640_000_000_000_000.0)
            .then_some(millis as i64);
    }
    let text = value.as_str()?.trim();
    DateTime::parse_from_rfc3339(text)
        .or_else(|_| DateTime::parse_from_rfc2822(text))
        .ok()
        .map(|time| time.timestamp_millis())
}

fn has_tool_result(entry: &Value) -> bool {
    entry
        .pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        _ => true,
    }
}

fn trimmed(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{name}>"))? + name.len() + 2;
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(&text[start..end])
}

fn noise(text: &str) -> bool {
    text == "No response requested."
        || (text.starts_with("[Request interrupted by user")
            && text.ends_with(']')
            && !text[..text.len() - 1].contains(']'))
        || (text.starts_with("<local-command-stdout>") && text.ends_with("</local-command-stdout>"))
}

fn user_text(entry: &Value) -> Option<String> {
    let message = entry.get("message")?;
    let content = message.get("content");
    let text = if let Some(text) = content.and_then(Value::as_str) {
        Some(text)
    } else if let Some(text) = message.get("text").and_then(Value::as_str) {
        Some(text)
    } else {
        content.and_then(Value::as_array).and_then(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .find(|text| !text.trim().is_empty() && !noise(text.trim()))
        })
    }?;
    let text = text.trim();
    if text.is_empty() || noise(text) {
        return None;
    }
    if let Some(command_message) = tag(text, "command-message") {
        let command = tag(text, "command-name")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(command_message.trim());
        if command.is_empty() {
            return None;
        }
        let mut command = if command.starts_with('/') {
            command.to_owned()
        } else {
            format!("/{command}")
        };
        if let Some(args) = tag(text, "command-args")
            .map(str::trim)
            .filter(|args| !args.is_empty())
        {
            command.push(' ');
            command.push_str(args);
        }
        Some(command)
    } else {
        Some(text.to_owned())
    }
}

fn preview(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf16(&normalized, 160)
}

fn truncate_utf16(text: &str, limit: usize) -> String {
    let mut length = 0;
    let end = text
        .char_indices()
        .find_map(|(index, c)| {
            length += c.len_utf16();
            (length > limit).then_some(index)
        })
        .unwrap_or(text.len());
    text[..end].to_owned()
}

async fn descriptor(path: &Path, modified: SystemTime) -> Result<Option<Value>> {
    let mut session_id = None;
    let mut cwd = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut first_prompt = None;
    let mut first_preview = None;
    let mut last_preview = None;
    visit_records(path, |entry| {
        if truthy(entry.get("isSidechain")) {
            return;
        }
        let kind = entry.get("type").and_then(Value::as_str);
        if kind == Some("user")
            && (entry.get("isSynthetic") == Some(&Value::Bool(true))
                || entry.get("isMeta") == Some(&Value::Bool(true))
                || truthy(entry.get("toolUseResult")))
        {
            return;
        }
        if session_id.as_deref().is_none_or(str::is_empty) {
            session_id = entry
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if cwd.as_deref().is_none_or(str::is_empty) {
            cwd = entry.get("cwd").and_then(Value::as_str).map(str::to_owned);
        }
        match kind {
            Some("custom-title") if entry.get("customTitle").is_some_and(Value::is_string) => {
                custom_title = trimmed(entry.get("customTitle")).map(str::to_owned)
            }
            Some("ai-title") if entry.get("aiTitle").is_some_and(Value::is_string) => {
                ai_title = trimmed(entry.get("aiTitle")).map(str::to_owned)
            }
            Some("user") => {
                if let Some(text) = user_text(&entry) {
                    let value = preview(&text);
                    if first_prompt.is_none() {
                        first_prompt = Some(text);
                        first_preview = Some(value.clone());
                    }
                    last_preview = Some(value);
                }
            }
            _ => {}
        }
    })
    .await?;
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
        return Ok(None);
    };
    let title = custom_title
        .or(ai_title)
        .or(first_prompt)
        .unwrap_or_else(|| format!("Claude session {}", truncate_utf16(&session_id, 8)));
    Ok(Some(
        json!({"providerHandleId":session_id,"cwd":cwd,"title":title,"firstPromptPreview":first_preview,"lastPromptPreview":last_preview,"lastActivityAt":DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Millis,true)}),
    ))
}

pub async fn list_importable_sessions(
    env: &BTreeMap<String, String>,
    options: &Value,
) -> Result<Vec<Value>> {
    tokio::time::timeout(
        std::time::Duration::from_secs(45),
        list_importable_inner(env, options),
    )
    .await
    .map_err(|_| anyhow!("Claude session discovery timed out after 45 seconds"))?
}

async fn list_importable_inner(
    env: &BTreeMap<String, String>,
    options: &Value,
) -> Result<Vec<Value>> {
    let cwd = options
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty());
    let root = match cwd {
        Some(cwd) => project_dir(cwd, env).await?,
        None => config_dir(env)?.join("projects"),
    };
    let root_entries = match directory_entries(&root).await {
        Ok(entries) => entries,
        Err(error) if missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let limit = options
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .min(usize::MAX as u64) as usize;
    let scan_limit = options
        .get("scanLimit")
        .and_then(Value::as_u64)
        .unwrap_or((limit as u64).saturating_mul(3))
        .min(500) as usize;
    let mut files = Vec::new();
    let mut denied = Vec::new();
    if cwd.is_some() {
        files.extend(
            root_entries
                .into_iter()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")),
        );
    } else {
        for entry in root_entries {
            let path = entry.path();
            match fs::metadata(&path).await {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if expected_import_error(&error) => {
                    if !missing(&error) {
                        denied.push(error);
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            match directory_entries(&path).await {
                Ok(entries) => files.extend(
                    entries
                        .into_iter()
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                        }),
                ),
                Err(error) if expected_import_error(&error) => {
                    if !missing(&error) {
                        denied.push(error);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut candidates = Vec::new();
    for file in files {
        match fs::metadata(&file).await {
            Ok(metadata) if metadata.is_file() => candidates.push((file, metadata.modified()?)),
            Ok(_) => {}
            Err(error) if expected_import_error(&error) => {
                if !missing(&error) {
                    denied.push(error);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    if candidates.is_empty() && !denied.is_empty() {
        bail!(
            "Cannot discover Claude sessions: access was denied to {} paths and no transcripts could be discovered. {}",
            denied.len(),
            denied[0]
        );
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    candidates.truncate(scan_limit);
    let count = candidates.len();
    let mut unreadable = Vec::new();
    let mut result = Vec::new();
    for (path, modified) in candidates {
        match descriptor(&path, modified).await {
            Ok(Some(value)) => result.push(value),
            Ok(None) => {}
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(expected_import_error) =>
            {
                unreadable.push(error)
            }
            Err(error) => return Err(error),
        }
    }
    if !unreadable.is_empty() && unreadable.len() == count {
        bail!(
            "Cannot list Claude sessions: all {count} candidate transcripts have unreadable import metadata. {}",
            unreadable[0]
        );
    }
    result.truncate(limit);
    Ok(result)
}

pub async fn renamed_title(
    cwd: &str,
    session_id: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let path = transcript_path(cwd, session_id, env).await?;
    let mut title = None;
    let result = visit_records(&path, |entry| {
        if !truthy(entry.get("isSidechain"))
            && entry.get("type").and_then(Value::as_str) == Some("custom-title")
        {
            if let Some(value) = trimmed(entry.get("customTitle")) {
                title = Some(value.to_owned());
            }
        }
    })
    .await;
    match result {
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
        Ok(_) => Ok(title),
    }
}

fn valid_sdk_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn sdk_fork_title(entries: &[Value]) -> String {
    let mut custom = None;
    let mut ai = None;
    for entry in entries {
        if let Some(title) = entry
            .get("customTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            custom = Some(title);
        }
        if let Some(title) = entry
            .get("aiTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            ai = Some(title);
        }
    }
    if let Some(title) = custom.or(ai) {
        return format!("{title} (fork)");
    }
    let mut command = String::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("user")
            || entry.get("isMeta") == Some(&Value::Bool(true))
            || entry.get("isCompactSummary") == Some(&Value::Bool(true))
            || has_tool_result(entry)
        {
            continue;
        }
        let content = entry.pointer("/message/content");
        let parts: Vec<&str> = match content {
            Some(Value::String(text)) => vec![text],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect(),
            _ => continue,
        };
        for part in parts {
            let text = part.replace('\n', " ");
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(name) = tag(text, "command-name") {
                if command.is_empty() {
                    command = name.to_owned();
                }
                continue;
            }
            if let Some(input) = tag(text, "bash-input") {
                return format!("! {} (fork)", input.trim());
            }
            // SDK's aee excludes markup/system scaffolding and interruption messages.
            let bytes = text.as_bytes();
            let markup = bytes.first() == Some(&b'<')
                && bytes.get(1).is_some_and(u8::is_ascii_lowercase)
                && {
                    let end = bytes[2..]
                        .iter()
                        .position(|byte| {
                            !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'-'
                        })
                        .map(|index| index + 2);
                    end.is_some_and(|index| {
                        bytes[index] == b'>' || bytes[index].is_ascii_whitespace()
                    })
                };
            if markup || (text.starts_with("[Request interrupted by user") && text.contains(']')) {
                continue;
            }
            let title = if text.encode_utf16().count() > 200 {
                format!("{}…", truncate_utf16(text, 200).trim())
            } else {
                text.to_owned()
            };
            return format!("{title} (fork)");
        }
    }
    format!(
        "{} (fork)",
        if command.is_empty() {
            "Forked session"
        } else {
            &command
        }
    )
}

/// Port of SDK 0.3.246 J8e -> $4 -> Vze/F4 -> z4 for Paseo's project-folder
/// SessionStore. This is deliberately not the SDK's default filesystem fork:
/// the configured store only has load/append, so it does not copy sidecars.
pub async fn fork_session(
    transcript_path: &Path,
    session_id: &str,
    up_to_message_id: Option<&str>,
) -> Result<String> {
    if !valid_sdk_uuid(session_id) {
        bail!("Invalid sessionId: {session_id}");
    }
    let up_to_message_id = up_to_message_id.filter(|id| !id.is_empty());
    if let Some(id) = up_to_message_id {
        if !valid_sdk_uuid(id) {
            bail!("Invalid upToMessageId: {id}");
        }
    }
    let directory = transcript_path
        .parent()
        .ok_or_else(|| anyhow!("Claude transcript has no project directory"))?;
    let source = directory.join(format!("{session_id}.jsonl"));
    let mut entries = Vec::new();
    if let Err(error) = visit_records(&source, |entry| {
        if entry.get("type").is_some_and(Value::is_string) {
            entries.push(entry);
        }
    })
    .await
    {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            bail!("Session {session_id} not found");
        }
        return Err(error);
    }
    if entries.is_empty() {
        bail!("Session {session_id} not found");
    }
    let title = sdk_fork_title(&entries);
    let mut transcript = Vec::new();
    let mut replacements = Vec::new();
    let mut suppressed = false;
    let mut atis = None;
    let mut relocated = None;
    for entry in entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("user" | "assistant" | "attachment" | "system" | "progress")
                if entry.get("uuid").is_some_and(Value::is_string) =>
            {
                if !truthy(entry.get("isSidechain")) {
                    transcript.push(entry);
                }
            }
            Some("history-suppression") => suppressed = true,
            Some("atis-latch")
                if entry.get("sessionId").and_then(Value::as_str) == Some(session_id) =>
            {
                if let Some(value) = entry
                    .get("atis")
                    .and_then(Value::as_str)
                    .filter(|value| value.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                {
                    atis = Some(value.to_owned());
                }
            }
            Some("content-replacement")
                if entry.get("sessionId").and_then(Value::as_str) == Some(session_id) =>
            {
                if let Some(values) = entry.get("replacements").and_then(Value::as_array) {
                    replacements.extend(values.iter().cloned());
                }
            }
            Some("relocated")
                if entry.get("sessionId").and_then(Value::as_str) == Some(session_id) =>
            {
                if let Some(value) = entry
                    .get("relocatedCwd")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    relocated = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    if transcript.is_empty() {
        bail!("Session {session_id} has no messages to fork");
    }
    if let Some(id) = up_to_message_id {
        let index = transcript
            .iter()
            .position(|entry| entry.get("uuid").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| anyhow!("Message {id} not found in session {session_id}"))?;
        transcript.truncate(index + 1);
    }
    let mut ids = HashMap::new();
    let mut positions = HashMap::new();
    for (index, entry) in transcript.iter().enumerate() {
        let id = entry["uuid"].as_str().expect("filtered UUID");
        ids.insert(id.to_owned(), Uuid::new_v4().to_string());
        positions.insert(id.to_owned(), index);
    }
    let last = transcript
        .iter()
        .rposition(|entry| entry.get("type").and_then(Value::as_str) != Some("progress"))
        .ok_or_else(|| anyhow!("Session {session_id} has no messages to fork"))?;
    let fork_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut output = Vec::new();
    if suppressed {
        output.push(json!({"type":"history-suppression","sessionId":fork_id,"cause":"fork_inherit","ts":now}));
    }
    for (index, entry) in transcript.iter().enumerate() {
        if entry.get("type").and_then(Value::as_str) == Some("progress") {
            continue;
        }
        let original_id = entry["uuid"].as_str().expect("filtered UUID");
        let mut parent = entry.get("parentUuid").and_then(Value::as_str);
        let mut mapped_parent = None;
        let mut visited = HashSet::new();
        while let Some(id) = parent.filter(|id| !id.is_empty()) {
            // Malformed cycles must not hang the daemon while replaying untrusted disk data.
            if !visited.insert(id) {
                bail!("Claude transcript contains a progress-parent cycle");
            }
            let Some(position) = positions.get(id) else {
                break;
            };
            let ancestor = &transcript[*position];
            if ancestor.get("type").and_then(Value::as_str) != Some("progress") {
                mapped_parent = ids.get(id);
                break;
            }
            parent = ancestor.get("parentUuid").and_then(Value::as_str);
        }
        let mut value = entry.clone();
        value["uuid"] = json!(ids[original_id]);
        value["parentUuid"] = json!(mapped_parent);
        if let Some(logical) = entry.get("logicalParentUuid") {
            value["logicalParentUuid"] = if logical.is_null() {
                Value::Null
            } else {
                json!(logical.as_str().and_then(|id| ids.get(id)))
            };
        }
        value["sessionId"] = json!(fork_id);
        value["isSidechain"] = Value::Bool(false);
        value["forkedFrom"] = json!({"sessionId":session_id,"messageUuid":original_id});
        if index == last {
            value["timestamp"] = json!(now);
        }
        if entry.get("type").and_then(Value::as_str) == Some("system")
            && entry.get("subtype").and_then(Value::as_str) == Some("model_refusal_fallback")
        {
            value["neutralizedByFork"] = Value::Bool(true);
        }
        let object = value.as_object_mut().expect("transcript object");
        for field in [
            "teamName",
            "agentName",
            "sessionKind",
            "slug",
            "sourceToolAssistantUUID",
        ] {
            object.remove(field);
        }
        output.push(value);
    }
    if !replacements.is_empty() {
        output.push(json!({"type":"content-replacement","sessionId":fork_id,"replacements":replacements,"uuid":Uuid::new_v4().to_string(),"timestamp":now}));
    }
    if let Some(atis) = atis {
        output.push(json!({"type":"atis-latch","sessionId":fork_id,"atis":atis}));
    }
    if let Some(cwd) = relocated {
        output.push(json!({"type":"relocated","sessionId":fork_id,"relocatedCwd":cwd}));
    }
    output.push(json!({"type":"custom-title","sessionId":fork_id,"customTitle":title,"uuid":Uuid::new_v4().to_string(),"timestamp":now}));
    let path = directory.join(format!("{fork_id}.jsonl"));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let mut line = Vec::new();
    for entry in output {
        serde_json::to_writer(&mut line, &entry)?;
        line.push(b'\n');
        file.write_all(&line).await?;
        line.clear();
    }
    file.flush().await?;
    Ok(fork_id)
}

#[cfg(test)]
mod tests {
    use super::super::projection;
    use super::*;

    fn lines(entries: &[Value]) -> String {
        entries.iter().map(|entry| format!("{entry}\n")).collect()
    }
    fn task(id: &str, agent: &str, description: &str) -> Value {
        json!({"type":"tool_use","id":id,"name":"Task",
            "input":{"subagent_type":agent,"description":description,"prompt":description}})
    }

    /// A parent that spawned file-backed agents A (finished) and B (unread: its
    /// file is a directory, so any read fails); A spawned C.
    async fn family() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(&path, lines(&[
            json!({"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"go"}}),
            json!({"type":"assistant","uuid":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant",
                "content":[task("toolu_1","reviewer","Review code"),task("toolu_2","scout","Scout")],"stop_reason":"tool_use"}}),
            json!({"type":"user","uuid":"u2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user",
                "content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}),
        ])).await.unwrap();
        let sidechains = directory.path().join("session").join("subagents");
        fs::create_dir_all(&sidechains).await.unwrap();
        fs::write(sidechains.join("agent-B.jsonl"), lines(&[
            json!({"type":"assistant","isSidechain":true,"agentId":"B","uuid":"s1","timestamp":"2026-01-01T00:00:03Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"sibling text"}],"stop_reason":"end_turn"}}),
        ])).await.unwrap();
        for (agent, meta) in [
            ("A", json!({"agentType":"reviewer","toolUseId":"toolu_1"})),
            ("B", json!({"agentType":"scout","toolUseId":"toolu_2"})),
            ("C", json!({"agentType":"digger","toolUseId":"toolu_3","spawnDepth":2})),
        ] {
            fs::write(sidechains.join(format!("agent-{agent}.meta.json")), meta.to_string()).await.unwrap();
        }
        fs::write(sidechains.join("agent-A.jsonl"), lines(&[
            json!({"type":"user","isSidechain":true,"agentId":"A","uuid":"c1","timestamp":"2026-01-01T00:00:03Z",
                "message":{"role":"user","content":"Review code"}}),
            json!({"type":"assistant","isSidechain":true,"agentId":"A","uuid":"c2","timestamp":"2026-01-01T00:00:04Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"child text"},task("toolu_3","digger","Dig")],
                    "stop_reason":"end_turn"}}),
        ])).await.unwrap();
        fs::write(sidechains.join("agent-C.jsonl"), lines(&[
            json!({"type":"assistant","isSidechain":true,"agentId":"C","uuid":"g1","timestamp":"2026-01-01T00:00:05Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"grandchild text"}],"stop_reason":"end_turn"}}),
        ])).await.unwrap();
        (directory, path)
    }

    fn children(events: &[Value]) -> Vec<&Value> {
        events.iter().filter(|e| e["type"] == "provider_subagent").map(|e| &e["event"]).collect()
    }

    #[tokio::test]
    async fn parent_history_announces_file_backed_agents_without_reading_them() {
        let (_directory, path) = family().await;
        let history = read_history(&path).await.unwrap();
        assert!(history.subagents.iter().all(|child| child.entries.is_empty() && child.file.is_some()));
        let events = projection::project_history("claude", &history);
        let announced = children(&events);
        assert!(announced.iter().all(|e| e["type"] == "upsert"), "{announced:?}");
        let ids: Vec<_> = announced.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["toolu_1", "toolu_1", "toolu_2"], "declare A, A finished, declare B");
        assert_eq!(announced[0]["source"]["agentId"], "A");
        assert_eq!(announced[0]["title"], "reviewer");
        assert_eq!(announced[0]["description"], "Review code");
        assert_eq!(announced[0]["timestamp"], "2026-01-01T00:00:01.000Z", "spawn time, not read from the body");
        assert_eq!(*announced[1], json!({"type":"upsert","id":"toolu_1","status":"completed"}));
        assert_eq!(announced[2]["source"]["agentId"], "B");
        // B's parent holds no result for it and its metadata has no status, so
        // nothing short of its body is authoritative: the recorded status stays.
        assert!(announced[2].get("status").is_none(), "{:?}", announced[2]);
        assert!(!events.iter().any(|e| e.to_string().contains("sibling text")));
        assert!(
            events.iter().any(|e| e["type"] == "timeline" && e["item"]["callId"] == "toolu_1"),
            "the parent's own Task card still replays"
        );
    }

    #[tokio::test]
    async fn opened_agent_replays_only_its_body_and_announces_its_children() {
        let (_directory, path) = family().await;
        let events = projection::project_history("claude", &read_history(&path).await.unwrap());
        let source = children(&events)
            .into_iter()
            .find(|e| e["id"] == "toolu_1" && e["source"].is_object())
            .unwrap()["source"]
            .clone();
        let (body, others, complete) = read_subagent(&path, "A").await.unwrap();
        assert!(complete);
        let opened = projection::project_subagent("claude", "toolu_1", &source, &body, &others, complete);
        assert!(opened.iter().all(|e| e["type"] == "provider_subagent"), "no parent row is replayed");
        let rows: Vec<_> = children(&opened).into_iter().filter(|e| e["type"] == "timeline").collect();
        assert!(rows.iter().all(|row| row["id"] == "toolu_1" && row["historyComplete"] == true), "{rows:?}");
        let texts: Vec<_> = rows
            .iter()
            .filter(|row| row["item"]["type"] == "assistant_message")
            .map(|row| row["item"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["child text"], "exactly the opened agent's reply");
        let tools: Vec<_> = rows
            .iter()
            .filter(|row| row["item"]["type"] == "tool_call")
            .map(|row| row["item"]["callId"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["toolu_3"], "its own spawn card, nothing of the parent's");
        assert!(!opened.iter().any(|e| ["go", "grandchild text", "sibling text"]
            .iter()
            .any(|text| e["event"]["item"]["text"] == *text)));
        assert!(
            children(&opened).iter().any(|e| e["type"] == "upsert" && e["id"] == "toolu_1" && e["status"] == "completed"),
            "the body's finished turn refreshes the status"
        );
        let nested = children(&opened)
            .into_iter()
            .find(|e| e["type"] == "upsert" && e["id"] == "toolu_3" && e["source"].is_object())
            .expect("the opened agent's own child is announced");
        assert_eq!(nested["parentSubagentId"], "toolu_1");
        assert_eq!(nested["source"]["agentId"], "C");
        assert!(!children(&opened).iter().any(|e| e["id"] == "toolu_2"), "siblings stay untouched");
        assert!(!opened.iter().chain(&events).any(|e| e.to_string().contains("sibling text")));
        // Opening B, whose parent recorded no outcome, takes its status from its body.
        let b = children(&events).into_iter().find(|e| e["id"] == "toolu_2").unwrap()["source"].clone();
        let (body, others, complete) = read_subagent(&path, "B").await.unwrap();
        let opened = projection::project_subagent("claude", "toolu_2", &b, &body, &others, complete);
        let texts: Vec<_> = children(&opened)
            .into_iter()
            .filter(|e| e["type"] == "timeline" && e["item"]["type"] == "assistant_message")
            .map(|e| e["item"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["sibling text"]);
        assert!(children(&opened).iter().any(|e| e["id"] == "toolu_2" && e["status"] == "completed"));
        fs::remove_file(path.with_extension("").join("subagents").join("agent-A.jsonl")).await.unwrap();
        let missing = read_subagent(&path, "A").await;
        assert!(missing.is_err(), "a missing body is an error, never an empty replay");
        assert!(read_subagent(&path, "unknown").await.is_err());
    }

    #[test]
    fn only_messages_with_a_stop_reason_are_final() {
        let record = |uuid: &str, id: &str, stop: Value| {
            json!({"type":"assistant","uuid":uuid,"message":{"id":id,"role":"assistant",
                "content":[{"type":"text","text":uuid}],"stop_reason":stop}})
        };
        let mut entries = vec![
            json!({"type":"user","uuid":"u1","message":{"role":"user","content":"go"}}),
            record("r1", "m1", Value::Null),
            record("r2", "m1", json!("tool_use")),
            // Not last, yet nothing records it as stopped: it is not final.
            record("r3", "m2", Value::Null),
            json!({"type":"user","uuid":"u2","message":{"role":"user","content":"more"}}),
            record("r4", "m3", Value::Null),
        ];
        let (finished, dropped) = keep_finished_messages(&mut entries);
        assert!(dropped);
        assert_eq!(finished, HashSet::from(["m1".to_owned()]));
        let kept: Vec<_> = entries.iter().map(|e| e["uuid"].as_str().unwrap()).collect();
        assert_eq!(kept, ["u1", "r1", "r2", "u2"]);
    }

    #[tokio::test]
    async fn held_sidechain_updates_survive_an_applied_snapshot_exactly_once() {
        use crate::providers::api::{
            ChildRowHold, EventSink, HistoryCoverage, ReplayOutcome, SubagentHistoryRequest,
            SubagentReplay, subagent_row,
        };
        let row = |item: Value| {
            json!({"type":"provider_subagent","provider":"claude","event":{"type":"timeline","id":"toolu_1","item":item}})
        };
        let pending = json!({"type":"tool_call","callId":"t1","name":"Bash","status":"running","detail":{"type":"shell","command":"ls"}});
        let completed = json!({"type":"tool_call","callId":"t1","name":"Bash","status":"completed","detail":{"type":"shell","command":"ls","output":"a"}});
        let failed = json!({"type":"tool_call","callId":"t2","name":"Bash","status":"failed","error":{"message":"boom"}});
        let finished_text = json!({"type":"assistant_message","text":"done.","messageId":"m1"});
        let finished_chunk = json!({"type":"assistant_message","text":".","messageId":"m1"});
        let streaming = json!({"type":"assistant_message","text":" more","messageId":"m2"});
        let (sink, mut published) = EventSink::channel(64, tokio_util::sync::CancellationToken::new());
        let hold = std::sync::Arc::new(ChildRowHold::default());
        let (decide, decision) = tokio::sync::watch::channel(None);
        let request = SubagentHistoryRequest {
            persistence: json!({"sessionId":"parent"}),
            config: json!({"cwd":"/work"}),
            child_id: "toolu_1".into(),
            source: Value::Null,
            status: json!("running"),
            replay: SubagentReplay::new("k".into(), decision),
        };
        let snapshot = vec![
            subagent_row("claude", "toolu_1", pending.clone(), None, HistoryCoverage::Unverified),
            subagent_row("claude", "toolu_1", finished_text, None, HistoryCoverage::Unverified),
        ];
        let (reading, read_started) = tokio::sync::oneshot::channel::<()>();
        let (finish, read_finished) = tokio::sync::oneshot::channel::<()>();
        let finished = HashSet::from(["m1".to_owned()]);
        let task = {
            let (hold, sink) = (hold.clone(), sink.clone());
            tokio::spawn(async move {
                let read = async move {
                    reading.send(()).unwrap();
                    read_finished.await.unwrap();
                    Ok::<_, anyhow::Error>((snapshot, HistoryCoverage::Unverified))
                };
                hold.publish_replay(&request, "claude", &sink, read, move |item, snapshot| {
                    super::super::covered_by_snapshot(item, snapshot, &finished)
                })
                .await
            })
        };
        read_started.await.unwrap();
        for item in [&pending, &completed, &failed, &finished_chunk, &streaming] {
            hold.emit(&sink, row(item.clone())).await.unwrap();
        }
        finish.send(()).unwrap();
        assert_eq!(published.recv().await.unwrap()["event"]["type"], "replay");
        decide.send(Some(ReplayOutcome::Applied)).unwrap();
        task.await.unwrap().unwrap();
        let mut after = Vec::new();
        while let Ok(event) = published.try_recv() {
            after.push(event["event"]["item"].clone());
        }
        assert_eq!(
            after,
            [completed, failed, streaming],
            "a newer tool status and a still-streaming message are published once; \
             only the identical tool row and the finished message's chunk are covered"
        );
    }
}
