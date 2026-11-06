//! One file tab target's file (Flutter `file_handle.dart`, original `file-panels/file/handle.ts`).
//!
//! The handle is sans-IO: it records what the file is and what this tab last read or wrote, and
//! emits [`HandleCmd`]s for the store to execute against the daemon. Results come back through
//! the `on_*` methods.
//!
//! The handle remembers the version it last read or wrote, and writes only over that version.
//! Changes on disk reach watchers once per version (`Changed` / `Deleted`); versions this handle
//! read or wrote itself do not. Jobs (reads and writes) run one at a time in FIFO order, so reads
//! wait for earlier writes; writes wait while the tab holds them. Once the file has a stat, the
//! snapshot keeps it; a deletion is reported only after a read confirms it.

use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

/// Original protocol `FileVersion` (`ready` | `missing` | `error`) with the `kind`/`mimeType`
/// classification attached to ready versions.
#[derive(Clone, Debug, PartialEq)]
pub enum FileVersion {
    Ready {
        size: u64,
        modified_at: String,
        revision: Option<String>,
        kind: Option<String>,
        mime_type: Option<String>,
    },
    Missing,
    Error(String),
}

impl FileVersion {
    pub fn from_json(json: &Value) -> FileVersion {
        match json.get("status").and_then(Value::as_str) {
            Some("ready") => FileVersion::Ready {
                size: json.get("size").and_then(Value::as_u64).unwrap_or(0),
                modified_at: json.get("modifiedAt").and_then(Value::as_str).unwrap_or("").to_owned(),
                revision: json.get("revision").and_then(Value::as_str).map(str::to_owned),
                kind: json.get("kind").and_then(Value::as_str).map(str::to_owned),
                mime_type: json.get("mimeType").and_then(Value::as_str).map(str::to_owned),
            },
            Some("missing") => FileVersion::Missing,
            _ => FileVersion::Error(
                json.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("File observation failed")
                    .to_owned(),
            ),
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, FileVersion::Ready { .. })
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, FileVersion::Missing)
    }

    pub fn is_error(&self) -> bool {
        matches!(self, FileVersion::Error(_))
    }

    /// Original `versionKey`.
    pub fn key(&self) -> String {
        match self {
            FileVersion::Ready { size, modified_at, revision, .. } => {
                version_key(*size, modified_at, revision.as_deref())
            }
            FileVersion::Missing => "missing".to_owned(),
            FileVersion::Error(error) => format!("error:{error}"),
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            FileVersion::Ready { size, .. } => *size,
            _ => 0,
        }
    }

    pub fn kind(&self) -> Option<&str> {
        match self {
            FileVersion::Ready { kind, .. } => kind.as_deref(),
            _ => None,
        }
    }

    pub fn mime_type(&self) -> Option<&str> {
        match self {
            FileVersion::Ready { mime_type, .. } => mime_type.as_deref(),
            _ => None,
        }
    }

    fn base(&self) -> Option<BaseVersion> {
        match self {
            FileVersion::Ready { size, modified_at, revision, .. } => Some(BaseVersion {
                size: *size,
                modified_at: modified_at.clone(),
                revision: revision.clone(),
            }),
            _ => None,
        }
    }

    pub fn classified(&self, kind: &str, mime_type: &str) -> FileVersion {
        match self {
            FileVersion::Ready { size, modified_at, revision, .. } => FileVersion::Ready {
                size: *size,
                modified_at: modified_at.clone(),
                revision: revision.clone(),
                kind: Some(kind.to_owned()),
                mime_type: Some(mime_type.to_owned()),
            },
            other => other.clone(),
        }
    }
}

pub fn version_key(size: u64, modified_at: &str, revision: Option<&str>) -> String {
    match revision {
        Some(revision) => revision.to_owned(),
        None => format!("{size}:{modified_at}"),
    }
}

/// One read of the file: its bytes and the version they are.
#[derive(Clone, Debug)]
pub struct FileContents {
    pub bytes: Arc<Vec<u8>>,
    pub size: u64,
    pub modified_at: String,
    pub revision: Option<String>,
    /// `text` | `image` | `binary`.
    pub kind: String,
    pub mime_type: String,
}

impl FileContents {
    pub fn key(&self) -> String {
        version_key(self.size, &self.modified_at, self.revision.as_deref())
    }

    /// The stat of a host without file observation (COMPAT workspaceFileEditing).
    pub fn as_version(&self) -> FileVersion {
        FileVersion::Ready {
            size: self.size,
            modified_at: self.modified_at.clone(),
            revision: self.revision.clone(),
            kind: Some(self.kind.clone()),
            mime_type: Some(self.mime_type.clone()),
        }
    }
}

/// Original `FileHandleSnapshot`.
#[derive(Clone, Debug, PartialEq)]
pub enum Snapshot {
    Loading,
    Ready(FileVersion),
    Missing,
    Error(String),
}

impl Snapshot {
    pub fn status(&self) -> &'static str {
        match self {
            Snapshot::Loading => "loading",
            Snapshot::Ready(_) => "ready",
            Snapshot::Missing => "missing",
            Snapshot::Error(_) => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BaseVersion {
    pub size: u64,
    pub modified_at: String,
    pub revision: Option<String>,
}

/// Who asked for a job; the session routes the outcome there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Purpose {
    ModelReload(u64),
    ModelReconcile(u64),
    ModelWrite(u64),
    /// A modified buffer written while its panel unmounts (original `dispose({ flush: true })`).
    Flush,
    TextReader(u64),
    BytesReader(u64),
}

#[derive(Clone, Debug)]
enum JobKind {
    ReadText,
    ReadBytes,
    Write { content: String, force: bool },
}

#[derive(Clone, Debug)]
struct Job {
    id: u64,
    kind: JobKind,
    purpose: Purpose,
}

#[derive(Clone, Debug)]
struct Running {
    job: Job,
    attempt: u32,
}

/// IO the store performs for a handle.
#[derive(Clone, Debug, PartialEq)]
pub enum HandleCmd {
    /// Start observing (fs.file.subscribe) for this generation.
    Observe { generation: u64 },
    /// Stop the observation of this generation.
    StopObserve { generation: u64 },
    /// Read the whole file for a queued job.
    Read { job: u64 },
    /// Read the file to confirm a deletion (outside the job queue).
    ConfirmRead,
    /// One `fs.file.write.request` attempt.
    Write { job: u64, content: String, expected: BaseVersion },
}

/// A write failure (original `PluginFileConflictError` / `PluginFileDeletedError`).
#[derive(Clone, Debug, PartialEq)]
pub enum WriteError {
    Conflict,
    Deleted,
    Failed(String),
    Closed,
}

impl WriteError {
    pub fn message(&self) -> String {
        match self {
            WriteError::Conflict => "The file changed on disk.".to_owned(),
            WriteError::Deleted => "The file was deleted.".to_owned(),
            WriteError::Failed(message) => message.clone(),
            WriteError::Closed => "The file tab was closed.".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum JobResult {
    Text(String),
    Bytes(Arc<Vec<u8>>),
    Written,
    ReadFailed(String),
    WriteFailed(WriteError),
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub purpose: Purpose,
    pub result: JobResult,
}

/// What watchers hear about the file on disk, plus snapshot changes for the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleEvent {
    Changed,
    Deleted,
    Snapshot,
}

#[derive(Default)]
pub struct FileHandle {
    snapshot_: Option<Snapshot>,
    stat: Option<FileVersion>,
    latest: Option<FileVersion>,
    base: Option<BaseVersion>,
    seen: HashSet<String>,
    deleted: bool,
    confirming: bool,
    has_bom: bool,
    queue: VecDeque<Job>,
    running: Option<Running>,
    next_job: u64,
    writing: u32,
    holds: HashSet<u64>,
    next_hold: u64,
    observing: Option<u64>,
    generation: u64,
    /// The kind the first read of this observation classified (COMPAT fileVersionKind).
    observed_kind: Option<(String, String)>,
    disposed: bool,
    pub events: Vec<HandleEvent>,
    pub cmds: Vec<HandleCmd>,
    pub outcomes: Vec<Outcome>,
}

impl FileHandle {
    pub fn new() -> FileHandle {
        FileHandle::default()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_.clone().unwrap_or(Snapshot::Loading)
    }

    /// The latest stat this handle knows, or `None` before the first one.
    pub fn stat(&self) -> Option<&FileVersion> {
        self.stat.as_ref()
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    /// Every job queued so far finished (original `settled`).
    pub fn settled(&self) -> bool {
        self.queue.is_empty() && self.running.is_none()
    }

    pub fn observing_generation(&self) -> Option<u64> {
        self.observing
    }

    /// Observes the file while the tab is on screen. Coming back reports what changed meanwhile.
    pub fn set_observing(&mut self, observing: bool) {
        if self.disposed || observing == self.observing.is_some() {
            return;
        }
        if !observing {
            self.stop_observing();
            return;
        }
        self.generation += 1;
        self.observing = Some(self.generation);
        self.observed_kind = None;
        self.cmds.push(HandleCmd::Observe { generation: self.generation });
    }

    /// Observes again after a failure.
    pub fn retry(&mut self) {
        self.stop_observing();
        self.set_observing(true);
    }

    fn stop_observing(&mut self) {
        if let Some(generation) = self.observing.take() {
            self.cmds.push(HandleCmd::StopObserve { generation });
        }
    }

    /// Classifies an observed version with the kind of this observation (COMPAT fileVersionKind:
    /// older hosts classify a file only when it is read; the first read's kind stands for later
    /// versions).
    pub fn classify(&mut self, version: FileVersion) -> FileVersion {
        if !version.is_ready() {
            return version;
        }
        if let (Some(kind), Some(mime)) = (version.kind(), version.mime_type()) {
            self.observed_kind = Some((kind.to_owned(), mime.to_owned()));
        }
        match &self.observed_kind {
            Some((kind, mime)) => version.classified(kind, mime),
            None => FileVersion::Error("The file has no kind.".to_owned()),
        }
    }

    /// The read that classified an observation's initial version.
    pub fn set_observed_kind(&mut self, kind: &str, mime_type: &str) {
        self.observed_kind = Some((kind.to_owned(), mime_type.to_owned()));
    }

    /// The observation of `generation` started with `initial` (already classified) or failed.
    pub fn on_observed(&mut self, generation: u64, initial: Result<FileVersion, String>) {
        if self.observing != Some(generation) || self.disposed {
            return;
        }
        match initial {
            Ok(version) => self.receive(version),
            Err(error) => {
                self.observing = None;
                self.set_snapshot(Snapshot::Error(error));
            }
        }
    }

    /// An `fs.file.update` (or a restored subscription's snapshot) for `generation`.
    pub fn on_observation(&mut self, generation: u64, version: FileVersion) {
        if self.observing == Some(generation) && !self.disposed {
            let version = self.classify(version);
            self.receive(version);
        }
    }

    fn receive(&mut self, observation: FileVersion) {
        if self.disposed {
            return;
        }
        self.latest = Some(observation.clone());
        if observation.is_ready() {
            self.stat = Some(observation.clone());
            if self.base.is_none() {
                self.base = observation.base();
                if let Some(base) = &self.base {
                    self.seen.insert(version_key(base.size, &base.modified_at, base.revision.as_deref()));
                }
            }
            self.set_snapshot(Snapshot::Ready(observation.clone()));
        } else if self.stat.is_none() {
            self.set_snapshot(match &observation {
                FileVersion::Missing => Snapshot::Missing,
                FileVersion::Error(error) => Snapshot::Error(error.clone()),
                FileVersion::Ready { .. } => unreachable!(),
            });
        }
        self.notify_change(&observation);
    }

    /// Tells watchers about a disk version this handle did not read or write.
    fn notify_change(&mut self, observation: &FileVersion) {
        if self.writing > 0 || observation.is_error() {
            return;
        }
        if observation.is_missing() {
            if !self.deleted {
                self.confirm_deletion();
            }
            return;
        }
        self.notify_version(observation.key());
    }

    fn notify_version(&mut self, key: String) {
        let restored = self.deleted;
        self.deleted = false;
        if self.seen.contains(&key) && !restored {
            return;
        }
        self.seen.insert(key);
        self.events.push(HandleEvent::Changed);
    }

    fn confirm_deletion(&mut self) {
        if self.confirming {
            return;
        }
        self.confirming = true;
        self.cmds.push(HandleCmd::ConfirmRead);
    }

    /// The confirming read: contents, or `None` when it failed (the failure is the confirmation).
    pub fn on_confirm_read(&mut self, contents: Option<FileContents>) {
        self.confirming = false;
        if self.disposed || self.writing > 0 || self.deleted {
            return;
        }
        if let Some(contents) = contents {
            self.notify_version(contents.key());
            return;
        }
        if !self.latest.as_ref().is_some_and(FileVersion::is_missing) {
            return;
        }
        self.deleted = true;
        self.events.push(HandleEvent::Deleted);
    }

    /// Holds this file's writes while the tab asks whether to discard unsaved changes.
    pub fn hold_writes(&mut self) -> u64 {
        self.next_hold += 1;
        self.holds.insert(self.next_hold);
        self.next_hold
    }

    /// Lets held writes run.
    pub fn resume_writes(&mut self, hold: u64) {
        if self.holds.remove(&hold) && self.holds.is_empty() {
            self.pump();
        }
    }

    pub fn read_text(&mut self, purpose: Purpose) {
        self.enqueue(JobKind::ReadText, purpose);
    }

    pub fn read_bytes(&mut self, purpose: Purpose) {
        self.enqueue(JobKind::ReadBytes, purpose);
    }

    /// Writes the file if it has not changed since this handle last read or wrote it; otherwise
    /// fails with `Conflict` (`force` writes anyway) or `Deleted`. A byte order mark read from
    /// the file is written back.
    pub fn write_text(&mut self, text: String, force: bool, purpose: Purpose) {
        let content = if self.has_bom { format!("\u{feff}{text}") } else { text };
        self.enqueue(JobKind::Write { content, force }, purpose);
    }

    fn enqueue(&mut self, kind: JobKind, purpose: Purpose) {
        if self.disposed {
            self.outcomes.push(Outcome { purpose, result: closed_result(&kind) });
            return;
        }
        self.next_job += 1;
        self.queue.push_back(Job { id: self.next_job, kind, purpose });
        self.pump();
    }

    fn pump(&mut self) {
        while self.running.is_none() && !self.disposed {
            let Some(job) = self.queue.front() else { return };
            if matches!(job.kind, JobKind::Write { .. }) && !self.holds.is_empty() {
                return;
            }
            let job = self.queue.pop_front().unwrap();
            match &job.kind {
                JobKind::ReadText | JobKind::ReadBytes => {
                    self.cmds.push(HandleCmd::Read { job: job.id });
                    self.running = Some(Running { job, attempt: 0 });
                }
                JobKind::Write { content, force } => {
                    self.writing += 1;
                    let expected = if *force {
                        match &self.stat {
                            Some(stat) => stat.base(),
                            None => self.base.clone(),
                        }
                    } else {
                        self.base.clone()
                    };
                    match expected {
                        Some(expected) => {
                            self.cmds.push(HandleCmd::Write {
                                job: job.id,
                                content: content.clone(),
                                expected,
                            });
                            self.running = Some(Running { job, attempt: 0 });
                        }
                        None => {
                            let purpose = job.purpose;
                            self.finish_write(purpose, JobResult::WriteFailed(WriteError::Conflict));
                        }
                    }
                }
            }
        }
    }

    /// The result of the `Read` command for `job`.
    pub fn on_read(&mut self, job: u64, result: Result<FileContents, String>) {
        let Some(running) = self.running.take_if(|running| running.job.id == job) else {
            return;
        };
        let purpose = running.job.purpose;
        let outcome = match result {
            _ if self.disposed => closed_result(&running.job.kind),
            Err(error) => JobResult::ReadFailed(error),
            Ok(contents) => {
                self.adopt(contents.size, &contents.modified_at, contents.revision.as_deref());
                match running.job.kind {
                    JobKind::ReadText => {
                        let bytes = contents.bytes.as_slice();
                        self.has_bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
                        let body = if self.has_bom { &bytes[3..] } else { bytes };
                        JobResult::Text(String::from_utf8_lossy(body).into_owned())
                    }
                    _ => JobResult::Bytes(contents.bytes.clone()),
                }
            }
        };
        self.outcomes.push(Outcome { purpose, result: outcome });
        self.pump();
    }

    /// The `fs.file.write.response` result object (`written` | `conflict` | `error`) of one
    /// attempt, or the transport error.
    pub fn on_write(&mut self, job: u64, result: Result<Value, String>) {
        let Some(mut running) = self.running.take_if(|running| running.job.id == job) else {
            return;
        };
        let purpose = running.job.purpose;
        let JobKind::Write { content, force } = running.job.kind.clone() else {
            return;
        };
        let result = match result {
            Err(error) => JobResult::WriteFailed(WriteError::Failed(error)),
            Ok(_) if self.disposed => JobResult::WriteFailed(WriteError::Closed),
            Ok(result) => match result.get("status").and_then(Value::as_str) {
                Some("written") => {
                    self.adopt(
                        result.get("size").and_then(Value::as_u64).unwrap_or(0),
                        result.get("modifiedAt").and_then(Value::as_str).unwrap_or(""),
                        result.get("revision").and_then(Value::as_str),
                    );
                    JobResult::Written
                }
                Some("conflict") => {
                    let disk = FileVersion::from_json(result.get("version").unwrap_or(&Value::Null));
                    if force && running.attempt == 0
                        && let Some(expected) = disk.base() {
                            running.attempt = 1;
                            self.cmds.push(HandleCmd::Write { job, content, expected });
                            self.running = Some(running);
                            return;
                        }
                    if disk.is_missing() {
                        JobResult::WriteFailed(WriteError::Deleted)
                    } else {
                        JobResult::WriteFailed(WriteError::Conflict)
                    }
                }
                _ => JobResult::WriteFailed(WriteError::Failed(
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Save was not acknowledged")
                        .to_owned(),
                )),
            },
        };
        self.finish_write(purpose, result);
        self.pump();
    }

    fn finish_write(&mut self, purpose: Purpose, result: JobResult) {
        self.writing = self.writing.saturating_sub(1);
        self.outcomes.push(Outcome { purpose, result });
        // Observations that arrived during the write may be its own version.
        if let Some(latest) = self.latest.clone()
            && !self.disposed {
                self.notify_change(&latest);
            }
    }

    /// Records a version this handle read or wrote, so its observation does not notify.
    fn adopt(&mut self, size: u64, modified_at: &str, revision: Option<&str>) {
        let base = BaseVersion {
            size,
            modified_at: modified_at.to_owned(),
            revision: revision.map(str::to_owned),
        };
        let key = version_key(size, modified_at, revision);
        self.seen.insert(key.clone());
        self.base = Some(base);
        if let Some(stat) = &self.stat
            && stat.key() != key {
                let next = FileVersion::Ready {
                    size,
                    modified_at: modified_at.to_owned(),
                    revision: revision.map(str::to_owned),
                    kind: stat.kind().map(str::to_owned),
                    mime_type: stat.mime_type().map(str::to_owned),
                };
                self.stat = Some(next.clone());
                self.set_snapshot(Snapshot::Ready(next));
            }
    }

    fn set_snapshot(&mut self, next: Snapshot) {
        let current = self.snapshot();
        let same = match (&current, &next) {
            (Snapshot::Ready(a), Snapshot::Ready(b)) => {
                a.key() == b.key() && a.kind() == b.kind() && a.mime_type() == b.mime_type()
            }
            (Snapshot::Error(a), Snapshot::Error(b)) => a == b,
            (Snapshot::Loading, Snapshot::Loading) | (Snapshot::Missing, Snapshot::Missing) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.snapshot_ = Some(next);
        if !self.disposed {
            self.events.push(HandleEvent::Snapshot);
        }
    }

    /// Closing the tab: observation stops, held and queued jobs are dropped (their owners hear
    /// `Closed`), an in-flight request's late result is ignored.
    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.stop_observing();
        self.disposed = true;
        self.holds.clear();
        while let Some(job) = self.queue.pop_front() {
            self.outcomes.push(Outcome { purpose: job.purpose, result: closed_result(&job.kind) });
        }
    }
}

fn closed_result(kind: &JobKind) -> JobResult {
    match kind {
        JobKind::Write { .. } => JobResult::WriteFailed(WriteError::Closed),
        _ => JobResult::ReadFailed("The file tab was closed.".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A host file the tests drive by hand: its text, revision and existence.
    struct Disk {
        text: Vec<u8>,
        revision: String,
        exists: bool,
        writes: u32,
    }

    impl Disk {
        fn version(&self) -> FileVersion {
            if !self.exists {
                return FileVersion::Missing;
            }
            FileVersion::Ready {
                size: self.text.len() as u64,
                modified_at: format!("t-{}", self.revision),
                revision: Some(self.revision.clone()),
                kind: Some("text".into()),
                mime_type: Some("text/plain".into()),
            }
        }
        fn contents(&self) -> Result<FileContents, String> {
            if !self.exists {
                return Err("missing".into());
            }
            Ok(FileContents {
                bytes: Arc::new(self.text.clone()),
                size: self.text.len() as u64,
                modified_at: format!("t-{}", self.revision),
                revision: Some(self.revision.clone()),
                kind: "text".into(),
                mime_type: "text/plain".into(),
            })
        }
    }

    /// Runs every pending command against the disk until the handle is idle.
    fn run(handle: &mut FileHandle, disk: &mut Disk) {
        loop {
            let cmds = std::mem::take(&mut handle.cmds);
            if cmds.is_empty() {
                return;
            }
            for cmd in cmds {
                match cmd {
                    HandleCmd::Observe { generation } => {
                        let version = handle.classify(disk.version());
                        handle.on_observed(generation, Ok(version));
                    }
                    HandleCmd::StopObserve { .. } => {}
                    HandleCmd::Read { job } => handle.on_read(job, disk.contents()),
                    HandleCmd::ConfirmRead => handle.on_confirm_read(disk.contents().ok()),
                    HandleCmd::Write { job, content, expected } => {
                        disk.writes += 1;
                        let result = if !disk.exists {
                            json!({"status": "conflict", "version": {"status": "missing"}})
                        } else if expected.revision.as_deref() != Some(disk.revision.as_str()) {
                            json!({"status": "conflict", "version": {"status": "ready", "size": disk.text.len(), "modifiedAt": format!("t-{}", disk.revision), "revision": disk.revision}})
                        } else {
                            disk.text = content.into_bytes();
                            disk.revision = format!("w{}", disk.writes);
                            json!({"status": "written", "size": disk.text.len(), "modifiedAt": format!("t-{}", disk.revision), "revision": disk.revision})
                        };
                        handle.on_write(job, Ok(result));
                    }
                }
            }
        }
    }

    fn disk(text: &str) -> Disk {
        Disk { text: text.as_bytes().to_vec(), revision: "r1".into(), exists: true, writes: 0 }
    }

    #[test]
    fn missing_from_the_start_is_the_missing_load_state() {
        let mut disk = disk("x");
        disk.exists = false;
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        assert_eq!(handle.snapshot(), Snapshot::Missing);
    }

    #[test]
    fn own_writes_do_not_notify_other_versions_notify_once() {
        let mut disk = disk("one");
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        handle.events.clear();
        handle.read_text(Purpose::ModelReload(1));
        run(&mut handle, &mut disk);
        handle.write_text("two".into(), false, Purpose::ModelWrite(1));
        run(&mut handle, &mut disk);
        // The daemon observes the handle's own write.
        let generation = handle.observing_generation().unwrap();
        handle.on_observation(generation, disk.version());
        assert!(!handle.events.contains(&HandleEvent::Changed));
        disk.text = b"three".to_vec();
        disk.revision = "r9".into();
        handle.on_observation(generation, disk.version());
        handle.on_observation(generation, disk.version());
        let changed = handle.events.iter().filter(|e| **e == HandleEvent::Changed).count();
        assert_eq!(changed, 1);
    }

    #[test]
    fn deletion_is_reported_only_after_a_read_confirms_it() {
        let mut disk = disk("one");
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        handle.events.clear();
        disk.exists = false;
        let generation = handle.observing_generation().unwrap();
        handle.on_observation(generation, disk.version());
        assert!(!handle.events.contains(&HandleEvent::Deleted));
        run(&mut handle, &mut disk);
        assert!(handle.events.contains(&HandleEvent::Deleted));
        assert_eq!(handle.snapshot().status(), "ready");
    }

    #[test]
    fn unread_versions_conflict_and_force_retries_over_the_disk_version() {
        let mut disk = disk("one");
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        handle.read_text(Purpose::ModelReload(1));
        run(&mut handle, &mut disk);
        disk.revision = "other".into();
        handle.outcomes.clear();
        handle.write_text("mine".into(), false, Purpose::ModelWrite(1));
        run(&mut handle, &mut disk);
        assert!(matches!(handle.outcomes[0].result, JobResult::WriteFailed(WriteError::Conflict)));
        handle.write_text("mine".into(), true, Purpose::ModelWrite(2));
        run(&mut handle, &mut disk);
        assert_eq!(disk.text, b"mine");
    }

    #[test]
    fn byte_order_mark_is_dropped_and_written_back() {
        let mut disk = disk("\u{feff}hello");
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        handle.read_text(Purpose::ModelReload(1));
        run(&mut handle, &mut disk);
        assert!(matches!(&handle.outcomes[0].result, JobResult::Text(text) if text == "hello"));
        handle.write_text("bye".into(), false, Purpose::ModelWrite(1));
        run(&mut handle, &mut disk);
        assert_eq!(&disk.text[..3], &[0xef, 0xbb, 0xbf]);
    }

    #[test]
    fn held_writes_wait_and_dispose_drops_them() {
        let mut disk = disk("one");
        let mut handle = FileHandle::new();
        handle.set_observing(true);
        run(&mut handle, &mut disk);
        handle.read_text(Purpose::ModelReload(1));
        run(&mut handle, &mut disk);
        let hold = handle.hold_writes();
        handle.write_text("two".into(), false, Purpose::ModelWrite(1));
        run(&mut handle, &mut disk);
        assert_eq!(disk.writes, 0);
        handle.resume_writes(hold);
        run(&mut handle, &mut disk);
        assert_eq!(disk.writes, 1);
        handle.outcomes.clear();
        handle.hold_writes();
        handle.write_text("three".into(), false, Purpose::ModelWrite(2));
        handle.dispose();
        run(&mut handle, &mut disk);
        assert!(matches!(handle.outcomes[0].result, JobResult::WriteFailed(WriteError::Closed)));
        assert_eq!(disk.writes, 1);
    }
}
