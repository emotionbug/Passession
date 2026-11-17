use super::cron::Cadence;
use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Notify},
};

pub fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}
pub fn date(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Target {
    #[serde(rename = "agent")]
    Agent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    #[serde(rename = "new-agent")]
    NewAgent { config: Value },
}
impl Target {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Agent { agent_id } => {
                uuid::Uuid::parse_str(agent_id)?;
            }
            Self::NewAgent { config } => {
                ensure!(config.is_object(), "new-agent config must be an object");
                ensure!(config["provider"].is_string(), "Invalid provider");
                ensure!(
                    config["cwd"].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "cwd cannot be empty"
                );
                for key in ["model", "modeId", "thinkingOptionId"] {
                    if let Some(v) = config.get(key) {
                        ensure!(
                            v.as_str().is_some_and(|s| !s.trim().is_empty()),
                            "Invalid {key}"
                        );
                    }
                }
                if let Some(v) = config.get("title") {
                    ensure!(
                        v.is_null() || v.as_str().is_some_and(|s| !s.trim().is_empty()),
                        "Invalid title"
                    );
                }
                if let Some(v) = config.get("archiveOnFinish") {
                    ensure!(v.is_boolean(), "Invalid archiveOnFinish");
                }
                if let Some(v) = config.get("isolation") {
                    ensure!(
                        matches!(v.as_str(), Some("local" | "worktree")),
                        "Invalid isolation"
                    );
                }
                for key in ["providerOptions", "featureValues", "mcpServers"] {
                    if let Some(v) = config.get(key) {
                        ensure!(v.is_object(), "Invalid {key}");
                    }
                }
                if let Some(v) = config.get("systemPrompt") {
                    ensure!(v.is_string(), "Invalid systemPrompt");
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub scheduled_for: String,
    pub started_at: String,
    #[serde(deserialize_with = "nullable")]
    pub ended_at: Option<String>,
    pub status: String,
    #[serde(deserialize_with = "nullable")]
    pub agent_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<Value>,
    #[serde(deserialize_with = "nullable")]
    pub output: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: Cadence,
    pub target: Target,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "nullable")]
    pub next_run_at: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub last_run_at: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub paused_at: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub expires_at: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub max_runs: Option<u64>,
    pub runs: Vec<Run>,
}
impl Schedule {
    fn validate(&self) -> Result<()> {
        safe_id(&self.id)?;
        ensure!(!self.prompt.is_empty(), "Schedule prompt is required");
        // Stored cron syntax is validated when used, as in the source schema.
        if let Cadence::Every { every_ms } = &self.cadence {
            ensure!(*every_ms > 0, "Invalid everyMs");
        }
        ensure!(
            matches!(self.status.as_str(), "active" | "paused" | "completed"),
            "Invalid schedule status"
        );
        ensure!(
            self.max_runs != Some(0),
            "maxRuns must be a positive integer"
        );
        self.target.validate()?;
        for run in &self.runs {
            ensure!(
                matches!(run.status.as_str(), "running" | "succeeded" | "failed"),
                "Invalid schedule run status"
            );
            if let Some(id) = &run.agent_id {
                uuid::Uuid::parse_str(id)?;
            }
            ensure!(
                run.workspace_id
                    .as_ref()
                    .is_none_or(|v| v.is_null() || v.is_string()),
                "Invalid workspaceId"
            );
        }
        Ok(())
    }
    pub fn completed_runs(&self) -> u64 {
        self.runs.iter().filter(|r| r.status != "running").count() as u64
    }
    pub fn should_complete(&self, now: DateTime<Utc>) -> bool {
        self.expires_at
            .as_deref()
            .and_then(|s| date(s).ok())
            .is_some_and(|t| t <= now)
            || self.max_runs.is_some_and(|n| self.completed_runs() >= n)
    }
    pub fn complete(&mut self, now: DateTime<Utc>) {
        self.status = "completed".into();
        self.next_run_at = None;
        self.paused_at = None;
        self.updated_at = timestamp(now);
    }
    pub fn value(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

fn safe_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']),
        "Invalid schedule id"
    );
    Ok(())
}

#[derive(Default)]
struct Index {
    records: HashMap<String, Schedule>,
    due: BTreeSet<(DateTime<Utc>, String)>,
    files: HashMap<PathBuf, (Option<SystemTime>, u64)>,
}
impl Index {
    fn deadline(schedule: &Schedule) -> Option<DateTime<Utc>> {
        if schedule.status != "active" {
            return None;
        }
        let next = date(schedule.next_run_at.as_deref()?).unwrap_or(DateTime::<Utc>::MIN_UTC);
        if schedule
            .max_runs
            .is_some_and(|n| schedule.completed_runs() >= n)
        {
            return Some(DateTime::<Utc>::MIN_UTC);
        }
        Some(
            schedule
                .expires_at
                .as_deref()
                .and_then(|at| date(at).ok())
                .map_or(next, |at| at.min(next)),
        )
    }
    fn remove(&mut self, id: &str) {
        if let Some(previous) = self.records.remove(id)
            && let Some(at) = Self::deadline(&previous)
        {
            self.due.remove(&(at, id.to_owned()));
        }
    }
    fn insert(&mut self, schedule: Schedule) {
        self.remove(&schedule.id);
        if let Some(at) = Self::deadline(&schedule) {
            self.due.insert((at, schedule.id.clone()));
        }
        self.records.insert(schedule.id.clone(), schedule);
    }
}

pub struct Store {
    dir: PathBuf,
    // A single writer also serializes named identity upserts against ordinary mutations.
    pub writer: Mutex<()>,
    index: Mutex<Index>,
    reported_invalid: Mutex<HashSet<PathBuf>>,
    pub changed: Arc<Notify>,
    external_changed: Arc<AtomicBool>,
    external_paths: Arc<StdMutex<HashSet<PathBuf>>>,
    watch_failed: Arc<AtomicBool>,
    watcher: StdMutex<Option<notify::RecommendedWatcher>>,
}
impl Store {
    pub async fn open(dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&dir).await?;
        let changed = Arc::new(Notify::new());
        let external_changed = Arc::new(AtomicBool::new(false));
        let events = external_changed.clone();
        let watch_failed = Arc::new(AtomicBool::new(false));
        let failed = watch_failed.clone();
        let external_paths = Arc::new(StdMutex::new(HashSet::new()));
        let paths = external_paths.clone();
        let watched = dir.clone();
        let wake = changed.clone();
        let watcher = (|| -> notify::Result<_> {
            let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if matches!(&event, Ok(event) if matches!(event.kind, notify::EventKind::Access(_))) {
                    return;
                }
                match event {
                    Ok(event) => {
                        if event.paths.contains(&watched)
                            && matches!(event.kind, notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_)))
                        {
                            failed.store(true, Ordering::Release);
                        }
                        if event.paths.is_empty() || event.paths.contains(&watched) {
                            events.store(true, Ordering::Release);
                        } else {
                            let mut paths = paths.lock().unwrap_or_else(|error| error.into_inner());
                            paths.extend(event.paths.into_iter().filter(|path| path.extension().is_some_and(|extension| extension == "json")));
                            if paths.is_empty() { return; }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Schedule directory watch failed");
                        failed.store(true, Ordering::Release);
                        events.store(true, Ordering::Release);
                    }
                }
                wake.notify_one();
            })?;
            watcher.watch(&dir, RecursiveMode::NonRecursive)?;
            Ok(watcher)
        })().map_err(|error| {
            tracing::warn!(%error, "Schedule directory watch unavailable; checking metadata every 30 seconds");
            error
        }).ok();
        let store = Self {
            dir,
            writer: Mutex::new(()),
            index: Mutex::new(Index::default()),
            reported_invalid: Mutex::new(HashSet::new()),
            changed,
            external_changed,
            external_paths,
            watch_failed,
            watcher: StdMutex::new(watcher),
        };
        store.refresh().await?;
        Ok(store)
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        safe_id(id)?;
        Ok(self.dir.join(format!("{id}.json")))
    }
    pub async fn refresh_external(&self, poll: bool) -> Result<()> {
        let watching = self
            .watcher
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
            && !self.watch_failed.load(Ordering::Acquire);
        let paths = std::mem::take(
            &mut *self
                .external_paths
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let mut refresh =
            self.external_changed.swap(false, Ordering::AcqRel) || (!watching && poll);
        if !refresh {
            let index = self.index.lock().await;
            for path in paths {
                let stamp = match tokio::fs::metadata(&path).await {
                    Ok(metadata) => Some((metadata.modified().ok(), metadata.len())),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        self.external_changed.store(true, Ordering::Release);
                        return Err(error.into());
                    }
                };
                if stamp.as_ref() != index.files.get(&path) {
                    refresh = true;
                    break;
                }
            }
        }
        if !refresh {
            // Own atomic writes already updated these exact fingerprints.
            return Ok(());
        }
        if let Err(error) = self.refresh().await {
            self.external_changed.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
    /// Slow external-edit reconciliation: unchanged files are only stat'ed, never read,
    /// parsed, validated or sorted. The caller holds writer after initialization.
    pub async fn refresh(&self) -> Result<()> {
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        let mut index = self.index.lock().await;
        let mut seen = HashSet::new();
        let mut invalid = self.reported_invalid.lock().await;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_none_or(|s| s != "json") {
                continue;
            }
            let metadata = match entry.metadata().await {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            seen.insert(path.clone());
            let stamp = (metadata.modified().ok(), metadata.len());
            if index.files.get(&path) == Some(&stamp) {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let parsed = serde_json::from_slice::<Schedule>(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|schedule| {
                    schedule.validate()?;
                    ensure!(
                        self.path(&schedule.id)? == path,
                        "Schedule file identity mismatch"
                    );
                    Ok(schedule)
                });
            if let Some(id) = path.file_stem().and_then(|id| id.to_str()) {
                index.remove(id);
            }
            match parsed {
                Ok(schedule) => {
                    index.insert(schedule);
                    invalid.remove(&path);
                }
                Err(error) => {
                    if invalid.insert(path.clone()) {
                        tracing::error!(file=%path.display(),%error,"Skipping invalid schedule file");
                    }
                }
            }
            index.files.insert(path, stamp);
        }
        let removed = index
            .files
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in removed {
            if let Some(id) = path.file_stem().and_then(|id| id.to_str()) {
                index.remove(id);
            }
            index.files.remove(&path);
            invalid.remove(&path);
        }
        Ok(())
    }
    pub async fn next_deadline(&self, running: &HashSet<String>) -> Option<DateTime<Utc>> {
        self.index
            .lock()
            .await
            .due
            .iter()
            .find(|(_, id)| !running.contains(id))
            .map(|(at, _)| *at)
    }
    pub async fn due_ids(&self, now: DateTime<Utc>) -> Result<Vec<String>> {
        Ok(self
            .index
            .lock()
            .await
            .due
            .iter()
            .take_while(|(at, _)| *at <= now)
            .map(|(_, id)| id.clone())
            .collect())
    }
    pub async fn list(&self) -> Result<Vec<Schedule>> {
        let mut records = self
            .index
            .lock()
            .await
            .records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(records)
    }
    pub async fn get(&self, id: &str) -> Result<Option<Schedule>> {
        safe_id(id)?;
        Ok(self.index.lock().await.records.get(id).cloned())
    }
    pub async fn require(&self, id: &str) -> Result<Schedule> {
        self.get(id)
            .await?
            .ok_or_else(|| super::ScheduleNotFoundError(id.to_owned()).into())
    }
    pub async fn write(&self, s: &Schedule) -> Result<()> {
        s.validate()?;
        let path = self.path(&s.id)?;
        let temp = self
            .dir
            .join(format!(".{}.{}.tmp", s.id, uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(s)?;
        let result = async {
            let mut options = tokio::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temp).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temp, &path).await?;
            sync_directory(&self.dir).await?;
            let metadata = tokio::fs::metadata(&path).await?;
            let mut index = self.index.lock().await;
            index
                .files
                .insert(path, (metadata.modified().ok(), metadata.len()));
            index.insert(s.clone());
            self.changed.notify_one();
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => sync_directory(&self.dir).await?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let mut index = self.index.lock().await;
        index.remove(id);
        index.files.remove(&path);
        self.changed.notify_one();
        Ok(())
    }
}
async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        tokio::fs::File::open(path).await?.sync_all().await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
