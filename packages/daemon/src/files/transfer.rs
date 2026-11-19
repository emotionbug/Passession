use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    path::{MAIN_SEPARATOR, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Result, anyhow, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use serde_json::{Value, json};
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::{mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::transport::{OwnedOperation, RequestContext};

pub const FILE_BEGIN: u8 = 0x10;
pub const FILE_CHUNK: u8 = 0x11;
pub const FILE_END: u8 = 0x12;

const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024; // GUI composer attachment limit.
const SOURCE_UPLOADS: usize = 8;
const DAEMON_UPLOADS: usize = 32;
const SOURCE_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
const DAEMON_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
// The wire has no chunk acknowledgements. A full SDK upload (128 KiB chunks)
// fits even when its writer has not run; pathological fragmentation fails
// this transfer rather than stalling all traffic on the socket.
const UPLOAD_QUEUE: usize = MAX_UPLOAD_BYTES as usize / (128 * 1024) + 2;
pub(super) const UPLOAD_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Begin payloads are UTF-8 JSON, preceded by a big-endian u16 byte length.
pub fn encode_frame(opcode: u8, id: &str, payload: &[u8]) -> Result<Bytes> {
    ensure!(!id.is_empty(), "File transfer requestId is required");
    ensure!(id.len() <= 255, "File transfer requestId is too long");
    ensure!(
        matches!(opcode, FILE_BEGIN | FILE_CHUNK | FILE_END),
        "Invalid file transfer opcode"
    );
    if opcode == FILE_BEGIN {
        ensure!(
            payload.len() <= u16::MAX as usize,
            "FileBegin metadata is too long"
        );
        validate_metadata(payload)?;
    }
    ensure!(
        opcode != FILE_END || payload.is_empty(),
        "FileEnd must not contain a payload"
    );
    let mut frame = BytesMut::with_capacity(
        2 + id.len() + payload.len() + if opcode == FILE_BEGIN { 2 } else { 0 },
    );
    frame.put_u8(opcode);
    frame.put_u8(id.len() as u8);
    frame.extend_from_slice(id.as_bytes());
    if opcode == FILE_BEGIN {
        frame.put_u16(payload.len() as u16);
    }
    frame.extend_from_slice(payload);
    Ok(frame.freeze())
}

fn validate_metadata(payload: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(payload)?;
    ensure!(
        value.is_object()
            && value["mime"].as_str().is_some_and(|s| !s.is_empty())
            && value["size"].as_u64().is_some()
            && matches!(value["encoding"].as_str(), Some("utf-8" | "binary"))
            && value["modifiedAt"].is_string()
            && value.get("revision").is_none_or(Value::is_string)
            && value.get("fileName").is_none_or(Value::is_string),
        "Invalid FileBegin metadata"
    );
    Ok(())
}

struct Frame<'a> {
    opcode: u8,
    id: Cow<'a, str>,
    payload_offset: usize,
}

fn decode_frame(bytes: &[u8]) -> Option<Frame<'_>> {
    if bytes.len() < 2 || !matches!(bytes[0], FILE_BEGIN | FILE_CHUNK | FILE_END) {
        return None;
    }
    let length = bytes[1] as usize;
    if length == 0 || length > bytes.len() - 2 {
        return None;
    }
    let offset = 2 + length;
    let body = &bytes[offset..];
    match bytes[0] {
        FILE_BEGIN => {
            if body.len() < 2 || u16::from_be_bytes([body[0], body[1]]) as usize != body.len() - 2 {
                return None;
            }
            // TextDecoder replaces malformed UTF-8 before JSON parsing in the source protocol.
            validate_metadata(String::from_utf8_lossy(&body[2..]).as_bytes()).ok()?;
        }
        FILE_END if !body.is_empty() => return None,
        _ => {}
    }
    Some(Frame {
        opcode: bytes[0],
        id: String::from_utf8_lossy(&bytes[2..offset]),
        payload_offset: offset,
    })
}

enum Command {
    Begin,
    Chunk(Bytes),
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Streaming,
    Ended,
}

struct Ingress {
    phase: Phase,
    remaining: u64,
    failure: Option<&'static str>,
}

struct Slot {
    id: String,
    sender: mpsc::Sender<Command>,
    deadline: watch::Sender<Instant>,
    cancel: CancellationToken,
    failed: CancellationToken,
    ingress: Mutex<Ingress>,
}

impl Slot {
    async fn ingress_error(&self) -> anyhow::Error {
        self.failed.cancelled().await;
        anyhow!(
            self.ingress
                .lock()
                .unwrap()
                .failure
                .expect("Upload failure reason")
        )
    }
}

#[derive(Default)]
struct Usage {
    count: usize,
    bytes: u64,
}

#[derive(Default)]
struct Registry {
    uploads: HashMap<String, HashMap<String, Arc<Slot>>>,
    // Cancelled writers retain their reservation until their file is closed
    // and cleaned up, including writers replaced by the same request ID.
    usage: HashMap<String, Usage>,
    tasks: JoinSet<()>,
}

impl Registry {
    fn fits(&self, source: &str, size: u64) -> Result<(), &'static str> {
        if self.usage.get(source).is_some_and(|usage| {
            usage.count >= SOURCE_UPLOADS || usage.bytes + size > SOURCE_UPLOAD_BYTES
        }) {
            return Err("This connection has too many uploads or too many upload bytes in flight");
        }
        let (count, bytes) = self.usage.values().fold((0, 0), |(count, bytes), usage| {
            (count + usage.count, bytes + usage.bytes)
        });
        if count >= DAEMON_UPLOADS || bytes + size > DAEMON_UPLOAD_BYTES {
            return Err("The daemon has too many uploads or too many upload bytes in flight");
        }
        Ok(())
    }

    fn slot(&self, source: &str, request: &str) -> Option<&Arc<Slot>> {
        self.uploads.get(source)?.get(request)
    }

    fn remove(&mut self, source: &str, request: &str) -> Option<Arc<Slot>> {
        let slots = self.uploads.get_mut(source)?;
        let slot = slots.remove(request)?;
        if slots.is_empty() {
            self.uploads.remove(source);
        }
        Some(slot)
    }
}

pub struct Transfers {
    home: PathBuf,
    ttl: Duration,
    path_prefix: String,
    registry: Arc<Mutex<Registry>>,
    cancel: CancellationToken,
    #[cfg(test)]
    write_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Transfers {
    pub fn new(home: PathBuf, ttl: Duration) -> Self {
        let path_prefix = format!("{}{MAIN_SEPARATOR}", home.join("uploads").display());
        Self {
            home,
            ttl,
            path_prefix,
            registry: Arc::new(Mutex::new(Registry::default())),
            cancel: CancellationToken::new(),
            #[cfg(test)]
            write_gate: None,
        }
    }

    pub async fn begin(&self, context: &RequestContext, request: &Value) -> Result<()> {
        let request_id = string_field(request, "requestId")?;
        ensure!(
            !request_id.is_empty() && request_id.len() <= 255,
            "Invalid file transfer request ID"
        );
        let file_name = string_field(request, "fileName")?;
        let mime_type = string_field(request, "mimeType")?;
        ensure!(
            !file_name.is_empty() && !mime_type.is_empty(),
            "File name and MIME type are required"
        );
        let size = request["size"]
            .as_u64()
            .ok_or_else(|| anyhow!("Invalid upload size"))?;
        string_field(request, "modifiedAt")?;
        let rejection = {
            let mut registry = self.registry.lock().unwrap();
            ensure!(!self.cancel.is_cancelled(), "File transfers are shut down");
            while let Some(result) = registry.tasks.try_join_next() {
                result?;
            }
            let source = context.source_id();
            let rejection = if size > MAX_UPLOAD_BYTES {
                // Terminal drop wording uses the file name from the error message.
                Some(("FILE_TOO_LARGE", file_name))
            } else if mime_type.len() > u16::MAX as usize {
                // The MIME type must fit in the u16-length FileBegin metadata.
                Some((
                    "UPLOAD_LIMIT_EXCEEDED",
                    "Upload MIME type exceeds the transfer metadata limit",
                ))
            } else {
                registry
                    .fits(source, size)
                    .err()
                    .map(|error| ("UPLOAD_LIMIT_EXCEEDED", error))
            };
            if rejection.is_some() {
                if let Some(old) = registry.remove(source, request_id) {
                    old.cancel.cancel();
                }
                context.drain_file_transfer(request_id);
            } else {
                // Retain a source-owned operation; binary frames carry no request owner.
                context.bind_file_transfer(request_id)?;
                let operation = match context.retain_operation(&["file.upload.response"]) {
                    Ok(operation) => operation,
                    Err(error) => {
                        context.release_file_transfer(request_id);
                        return Err(error);
                    }
                };
                let id = format!("upload_{}", uuid::Uuid::new_v4());
                let file_name = sanitize_file_name(file_name);
                let directory = self.home.join("uploads").join(&id);
                let upload = Upload {
                    request_id: request_id.to_owned(),
                    id: id.clone(),
                    path: directory.join(&file_name),
                    directory,
                    file_name,
                    marker: upload_marker(&self.home, &id),
                    mime_type: mime_type.to_owned(),
                    size,
                    file: None,
                    created: false,
                    #[cfg(test)]
                    write_gate: self.write_gate.clone(),
                };
                let (sender, commands) = mpsc::channel(UPLOAD_QUEUE.min(size as usize + 2));
                let (deadline, deadlines) = watch::channel(Instant::now() + self.ttl);
                let slot = Arc::new(Slot {
                    id,
                    sender,
                    deadline,
                    cancel: self.cancel.child_token(),
                    failed: CancellationToken::new(),
                    ingress: Mutex::new(Ingress {
                        phase: Phase::Pending,
                        remaining: size,
                        failure: None,
                    }),
                });
                if let Some(old) = registry
                    .uploads
                    .entry(source.to_owned())
                    .or_default()
                    .insert(request_id.to_owned(), slot.clone())
                {
                    old.cancel.cancel();
                }
                let usage = registry.usage.entry(source.to_owned()).or_default();
                usage.count += 1;
                usage.bytes += size;
                registry.tasks.spawn(run_upload(
                    self.registry.clone(),
                    slot,
                    RequestContext::from_source(context.source.clone()),
                    operation,
                    upload,
                    commands,
                    deadlines,
                ));
            }
            rejection
        };
        if let Some((code, error)) = rejection {
            context.protocol_failure(request, code, error).await?;
        }
        Ok(())
    }

    pub fn receive(&self, context: &RequestContext, bytes: Bytes) -> bool {
        let Some(frame) = decode_frame(&bytes) else {
            return false;
        };
        let registry = self.registry.lock().unwrap();
        let Some(slot) = registry.slot(context.source_id(), &frame.id) else {
            return context.discarding_file_transfer(&frame.id);
        };
        let mut ingress = slot.ingress.lock().unwrap();
        if ingress.failure.is_some() || slot.cancel.is_cancelled() {
            return true;
        }
        let mut next_remaining = ingress.remaining;
        let mut next_phase = ingress.phase;
        let command = match (frame.opcode, ingress.phase) {
            (FILE_BEGIN, Phase::Pending) => {
                next_phase = Phase::Streaming;
                Ok(Command::Begin)
            }
            (FILE_CHUNK, Phase::Streaming) => {
                let len = bytes.len() - frame.payload_offset;
                if len == 0 {
                    return true;
                }
                if len as u64 <= ingress.remaining {
                    next_remaining -= len as u64;
                    Ok(Command::Chunk(bytes.slice(frame.payload_offset..)))
                } else {
                    Err("Upload exceeded its declared size")
                }
            }
            (FILE_END, Phase::Streaming) if ingress.remaining == 0 => {
                next_phase = Phase::Ended;
                Ok(Command::End)
            }
            (FILE_END, Phase::Streaming) => Err("Upload ended before reaching its declared size"),
            _ => Err("Upload frames arrived out of order"),
        };
        let result = command.and_then(|command| {
            slot.sender.try_send(command).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => "Upload transfer buffer is full",
                mpsc::error::TrySendError::Closed(_) => "Upload transfer is closed",
            })
        });
        match result {
            Ok(()) => {
                ingress.remaining = next_remaining;
                ingress.phase = next_phase;
                slot.deadline.send_replace(Instant::now() + self.ttl);
            }
            Err(error) => {
                ingress.failure = Some(error);
                slot.failed.cancel();
            }
        }
        true
    }

    pub(super) fn is_receiving(&self, id: &str) -> bool {
        self.registry
            .lock()
            .unwrap()
            .uploads
            .values()
            .any(|slots| slots.values().any(|slot| slot.id == id))
    }

    pub(super) fn referenced_uploads<'a>(&self, prompt: &'a Value) -> PromptUploads<'a> {
        fn visit<'a>(value: &'a Value, prefix: &str, explicit: bool, ids: &mut PromptUploads<'a>) {
            match value {
                Value::String(text) => {
                    for (offset, _) in text.match_indices(prefix) {
                        let rest = &text[offset + prefix.len()..];
                        let Some(id) = rest.get(..43).filter(|id| is_upload_id(id)) else {
                            continue;
                        };
                        if rest[43..].starts_with(MAIN_SEPARATOR) {
                            *ids.entry(id).or_default() |= explicit;
                        }
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        visit(value, prefix, false, ids);
                    }
                }
                Value::Object(values) => {
                    let kind = value["type"].as_str();
                    if kind == Some("image") {
                        return; // Image blocks contain base64, not file references.
                    }
                    for (key, value) in values {
                        visit(
                            value,
                            prefix,
                            key == "path" && kind == Some("uploaded_file"),
                            ids,
                        );
                    }
                }
                _ => {}
            }
        }

        let mut ids = HashMap::new();
        visit(prompt, &self.path_prefix, false, &mut ids);
        ids
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut tasks = {
            let mut registry = self.registry.lock().unwrap();
            self.cancel.cancel();
            std::mem::take(&mut registry.tasks)
        };
        let mut first_error = None;
        while let Some(result) = tasks.join_next().await {
            let result = result.map_err(anyhow::Error::from);
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for Transfers {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Upload {
    request_id: String,
    id: String,
    directory: PathBuf,
    marker: PathBuf,
    path: PathBuf,
    file_name: String,
    mime_type: String,
    size: u64,
    file: Option<File>,
    created: bool,
    #[cfg(test)]
    write_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Upload {
    async fn write(
        &mut self,
        commands: &mut mpsc::Receiver<Command>,
        deadlines: &mut watch::Receiver<Instant>,
        slot: &Slot,
        owner_cancel: &CancellationToken,
    ) -> Result<bool> {
        loop {
            let deadline = *deadlines.borrow_and_update();
            let command = tokio::select! { biased;
                _ = slot.cancel.cancelled() => return Ok(false),
                _ = owner_cancel.cancelled() => return Ok(false),
                error = slot.ingress_error() => return Err(error),
                _ = tokio::time::sleep_until(deadline) => {
                    let expired = *deadlines.borrow() <= Instant::now();
                    if expired {
                        return Err(anyhow!("Upload expired before completion"));
                    }
                    continue;
                }
                _ = deadlines.changed() => continue,
                command = commands.recv() => command,
            };
            match command {
                Some(Command::Begin) => {
                    fs::create_dir_all(self.directory.parent().unwrap()).await?;
                    fs::create_dir(&self.directory).await?;
                    self.created = true;
                    self.file = Some(
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&self.path)
                            .await?,
                    );
                }
                Some(Command::Chunk(bytes)) => {
                    #[cfg(test)]
                    if let Some(gate) = &self.write_gate {
                        tokio::select! { biased;
                            _ = slot.cancel.cancelled() => return Ok(false),
                            _ = owner_cancel.cancelled() => return Ok(false),
                            error = slot.ingress_error() => return Err(error),
                            permit = gate.acquire() => permit.unwrap().forget(),
                        }
                    }
                    tokio::select! { biased;
                        _ = slot.cancel.cancelled() => return Ok(false),
                        _ = owner_cancel.cancelled() => return Ok(false),
                        error = slot.ingress_error() => return Err(error),
                        result = self.file.as_mut().unwrap().write_all(&bytes) => result?,
                    }
                }
                Some(Command::End) => {
                    self.file.as_mut().unwrap().flush().await?;
                    return Ok(true);
                }
                None => return Ok(false),
            }
        }
    }
    fn response(&self, error: Option<&str>) -> Value {
        json!({"type":"file.upload.response", "payload":{
            "requestId":self.request_id,
            "file": if error.is_some() { Value::Null } else { json!({"type":"uploaded_file",
                "id":self.id,"fileName":self.file_name,"mimeType":self.mime_type,
                "size":self.size,"path":self.path}) }, "error":error }})
    }
    async fn cleanup(&self) -> Result<()> {
        if self.created {
            match fs::remove_dir_all(&self.directory).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            remove_marker(&self.marker).await?;
        }
        Ok(())
    }
}

async fn run_upload(
    registry: Arc<Mutex<Registry>>,
    slot: Arc<Slot>,
    context: RequestContext,
    operation: OwnedOperation,
    mut upload: Upload,
    mut commands: mpsc::Receiver<Command>,
    mut deadlines: watch::Receiver<Instant>,
) {
    let owner_cancel = operation.cancellation();
    let mut result = upload
        .write(&mut commands, &mut deadlines, &slot, &owner_cancel)
        .await;
    // Drop queued bytes promptly. Wait for any in-progress Tokio File operation
    // before closing the file or removing the directory (notably on Windows).
    drop(commands);
    if let Some(file) = upload.file.take() {
        drop(file.into_std().await);
    }
    let cancelled = || slot.cancel.is_cancelled() || owner_cancel.is_cancelled();
    if matches!(result, Ok(true)) && !cancelled() {
        result = write_upload_marker(&upload.marker, &UploadReferences::default())
            .await
            .map(|()| true);
    }
    let (current, complete) = {
        let mut registry = registry.lock().unwrap();
        // Finalize while ingress is locked out so a late failure cannot race
        // a success response.
        if let Some(failure) = slot.ingress.lock().unwrap().failure {
            result = Err(anyhow!(failure));
        }
        let complete = matches!(result, Ok(true)) && !cancelled();
        let source = context.source_id();
        let current = registry
            .slot(source, &upload.request_id)
            .is_some_and(|current| Arc::ptr_eq(current, &slot));
        if current {
            registry.remove(source, &upload.request_id);
            if !complete && !cancelled() {
                context.drain_file_transfer(&upload.request_id);
            } else {
                context.release_file_transfer(&upload.request_id);
            }
        }
        (current, complete)
    };
    let delivered = if complete && current {
        tokio::select! { biased;
            _ = slot.cancel.cancelled() => false,
            _ = owner_cancel.cancelled() => false,
            result = operation.emit(upload.response(None)) => result.is_ok(),
        }
    } else {
        false
    };
    if !delivered && let Err(error) = upload.cleanup().await {
        tracing::warn!(upload = %upload.id, %error, "Failed to clean up upload");
    }
    if current
        && !cancelled()
        && let Err(error) = result
    {
        let message = error.to_string();
        if let Err(error) = operation.emit(upload.response(Some(&message))).await {
            tracing::debug!(upload = %upload.id, %error, "Failed to send upload error");
        }
    }
    let _ = operation.release().await;
    let mut registry = registry.lock().unwrap();
    let usage = registry.usage.get_mut(context.source_id()).unwrap();
    usage.count -= 1;
    usage.bytes -= upload.size;
    if usage.count == 0 {
        registry.usage.remove(context.source_id());
    }
}

pub(super) type PromptUploads<'a> = HashMap<&'a str, bool>;

pub(super) fn is_upload_id(id: &str) -> bool {
    id.strip_prefix("upload_")
        .is_some_and(|uuid| uuid.len() == 36 && uuid::Uuid::try_parse(uuid).is_ok())
}

pub(super) fn upload_marker(home: &Path, id: &str) -> PathBuf {
    home.join("upload-references").join(format!("{id}.json"))
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct UploadReferences {
    legacy: bool,
    agents: BTreeSet<String>,
}

async fn read_upload_marker(path: &Path) -> Result<Option<UploadReferences>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn write_upload_marker(path: &Path, marker: &UploadReferences) -> Result<()> {
    crate::agent::storage::atomic_json(path, &serde_json::to_value(marker)?).await
}

async fn remove_marker(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// The caller serializes reads and mutations of a marker with the same upload's sweep.
pub(super) async fn retain_upload(
    directory: &Path,
    marker: &Path,
    agent: &str,
    explicit: bool,
) -> Result<()> {
    match fs::symlink_metadata(directory).await {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "Upload is not a directory: {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !explicit,
                "Uploaded attachment no longer exists: {}",
                directory.display()
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let mut references = read_upload_marker(marker)
        .await?
        .unwrap_or_else(|| UploadReferences {
            legacy: true,
            ..Default::default()
        });
    if !references.agents.contains(agent) {
        references.agents.insert(agent.to_owned());
        write_upload_marker(marker, &references).await?;
    }
    Ok(())
}

pub(super) async fn reclaim_upload<F>(
    directory: &Path,
    marker: &Path,
    now: SystemTime,
    protect_legacy: bool,
    owner_exists: impl Fn(String) -> F,
) -> Result<()>
where
    F: Future<Output = Result<bool>>,
{
    // Do not treat symlinks, unknown marker contents, inaccessible files, or
    // missing timestamps as evidence that an upload can be removed.
    let metadata = match fs::symlink_metadata(directory).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    let references = read_upload_marker(marker).await?;
    if protect_legacy && references.as_ref().is_none_or(|refs| refs.legacy) {
        return Ok(());
    }
    if let Some(refs) = references.filter(|refs| !refs.agents.is_empty()) {
        // Recheck each owner while the marker is locked; an older snapshot of
        // all agents could miss a newly created owner admitted during the sweep.
        for agent in refs.agents {
            if owner_exists(agent).await? {
                return Ok(());
            }
        }
    } else if !now
        .duration_since(metadata.modified()?)
        .is_ok_and(|age| age >= UPLOAD_RETENTION)
    {
        return Ok(());
    }
    fs::remove_dir_all(directory).await?;
    remove_marker(marker).await
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{key} is required"))
}

fn sanitize_file_name(value: &str) -> String {
    // Node's POSIX basename removes trailing slashes before selecting the final component.
    let basename = value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let replaced: String = basename
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = replaced.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let name = if name.is_empty() || name == "." || name == ".." {
        "upload"
    } else {
        name
    };
    if name.len() <= 255 {
        return name.to_owned();
    }
    let extension = name
        .rfind('.')
        .filter(|index| *index > 0)
        .map(|index| &name[index..])
        .unwrap_or("");
    let extension = if extension.len() < 255 { extension } else { "" };
    let mut end = 255 - extension.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = String::with_capacity(end + extension.len());
    result.push_str(&name[..end]);
    result.push_str(extension);
    result
}

struct DownloadEntry {
    path: PathBuf,
    file_name: String,
    mime_type: String,
    expires: Instant,
}

pub struct Downloads {
    ttl: Duration,
    tokens: Mutex<HashMap<String, DownloadEntry>>,
}

pub struct FileDownload {
    pub file: File,
    pub size: u64,
    pub mime_type: String,
    pub file_name: String,
    pub content_disposition: String,
}

impl Downloads {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Tokens bind the canonical path observed at issue time; `open` rejects the
    /// file if that path no longer canonicalizes to itself (a replaced leaf or
    /// parent). Callers resolve the request path and permissions beforehand.
    pub async fn issue(
        &self,
        path: PathBuf,
        _relative: String,
        file_name: String,
        mime_type: String,
        _size: u64,
    ) -> Result<String> {
        // The canonical form is platform-specific (`\\?\` verbatim paths on
        // Windows, `/private/var` on macOS), so it is computed here rather than
        // required of the caller's path spelling.
        let path = fs::canonicalize(&path).await?;
        ensure!(fs::metadata(&path).await?.is_file(), "File not found");
        let now = Instant::now();
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, entry| entry.expires > now);
        let token = uuid::Uuid::new_v4().to_string();
        tokens.insert(
            token.clone(),
            DownloadEntry {
                path,
                file_name,
                mime_type,
                expires: now + self.ttl,
            },
        );
        Ok(token)
    }

    /// None means an invalid/expired token (HTTP 403); an error means unavailable file (HTTP 404).
    /// Tokens are consumed even when opening their file fails.
    pub async fn open(&self, token: &str) -> Result<Option<FileDownload>> {
        let entry = self.tokens.lock().unwrap().remove(token);
        let Some(entry) = entry.filter(|entry| entry.expires > Instant::now()) else {
            return Ok(None);
        };
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(&entry.path).await?;
        let metadata = file.metadata().await?;
        ensure!(metadata.is_file(), "File not found");
        ensure!(
            fs::canonicalize(&entry.path).await? == entry.path,
            "File not found"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = fs::metadata(&entry.path).await?;
            ensure!(
                metadata.dev() == current.dev() && metadata.ino() == current.ino(),
                "File not found"
            );
        }
        Ok(Some(FileDownload {
            file,
            size: metadata.len(),
            content_disposition: content_disposition(&entry.file_name),
            mime_type: entry.mime_type,
            file_name: entry.file_name,
        }))
    }
}

fn content_disposition(name: &str) -> String {
    let mut result = String::from("attachment; filename=\"");
    let mut non_ascii = false;
    // JavaScript's non-Unicode regex replaces each UTF-16 code unit, including surrogate pairs.
    for unit in name.encode_utf16() {
        if !(0x20..=0x7e).contains(&unit) {
            result.push('?');
            non_ascii = true;
        } else {
            let ch = unit as u8 as char;
            if matches!(ch, '\\' | '"') {
                result.push('\\');
            }
            result.push(ch);
        }
    }
    result.push('"');
    let percent_escape = name
        .as_bytes()
        .windows(3)
        .any(|b| b[0] == b'%' && b[1].is_ascii_hexdigit() && b[2].is_ascii_hexdigit());
    if non_ascii || percent_escape {
        result.push_str("; filename*=UTF-8''");
        const HEX: &[u8] = b"0123456789ABCDEF";
        for byte in name.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~') {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push(HEX[(byte >> 4) as usize] as char);
                result.push(HEX[(byte & 15) as usize] as char);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::OutboundFrame;
    use std::{collections::HashSet, sync::RwLock};

    fn metadata() -> Vec<u8> {
        serde_json::to_vec(
            &json!({"mime":"text/plain","size":3,"encoding":"binary","modifiedAt":""}),
        )
        .unwrap()
    }

    fn request(name: &str, size: u64) -> Value {
        json!({"type":"file.upload.request","requestId":"same","fileName":name,
            "mimeType":"text/plain","size":size,"modifiedAt":""})
    }

    fn source(id: &str) -> crate::transport::SourceConfig {
        crate::transport::SourceConfig {
            id: id.into(),
            session_id: "session".into(),
            client_id: "client".into(),
            capabilities: json!({"owned_subscriptions":true}),
            principal: json!({}),
            permissions: Arc::new(RwLock::new(HashSet::from(["workspace.write".to_owned()]))),
            routed: false,
            app_version: None,
            cancel: CancellationToken::new(),
        }
    }

    async fn begin_request(transfers: &Transfers, context: &RequestContext, id: &str, size: u64) {
        let mut message = request("test.txt", size);
        message["requestId"] = json!(id);
        let requesting = context.for_request(message.clone());
        transfers.begin(&requesting, &message).await.unwrap();
        requesting.finish_request();
    }

    fn receive(transfers: &Transfers, source: &RequestContext, opcode: u8, id: &str, bytes: &[u8]) {
        assert!(transfers.receive(source, encode_frame(opcode, id, bytes).unwrap()));
    }

    fn slot(transfers: &Transfers, context: &RequestContext, request: &str) -> Arc<Slot> {
        transfers
            .registry
            .lock()
            .unwrap()
            .slot(context.source_id(), request)
            .unwrap()
            .clone()
    }

    fn response(frame: &OutboundFrame) -> Value {
        let OutboundFrame::Text(text) = frame else {
            panic!("Expected upload response");
        };
        serde_json::from_str::<Value>(text).unwrap()["message"].take()
    }

    async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fs::try_exists(path).await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn join_tasks(transfers: &Transfers) {
        let mut tasks = std::mem::take(&mut transfers.registry.lock().unwrap().tasks);
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn binary_wire_lengths_and_metadata_validation() {
        let payload = metadata();
        let bytes = encode_frame(FILE_BEGIN, "é", &payload).unwrap();
        assert_eq!(&bytes[..4], &[FILE_BEGIN, 2, 0xc3, 0xa9]);
        assert_eq!(&bytes[4..6], &(payload.len() as u16).to_be_bytes());
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded.id, "é");
        assert_eq!(decoded.opcode, FILE_BEGIN);
        assert!(encode_frame(FILE_CHUNK, "", b"").is_err());
        assert!(encode_frame(FILE_CHUNK, &"a".repeat(256), b"").is_err());
        assert!(encode_frame(FILE_BEGIN, "a", &vec![b' '; 65536]).is_err());
        assert!(
            encode_frame(
                FILE_BEGIN,
                "a",
                br#"{"mime":"","size":0,"encoding":"binary","modifiedAt":""}"#
            )
            .is_err()
        );
        assert!(encode_frame(FILE_END, "a", b"x").is_err());
        for bytes in [
            vec![],
            vec![FILE_BEGIN],
            vec![FILE_END, 0],
            vec![FILE_END, 1, b'a', 1],
            vec![FILE_BEGIN, 1, b'a', 0, 1],
            vec![0x99, 1, b'a'],
        ] {
            assert!(decode_frame(&bytes).is_none());
        }
        let mut trailing = encode_frame(FILE_BEGIN, "a", &payload).unwrap().to_vec();
        trailing.push(0);
        assert!(decode_frame(&trailing).is_none());
        assert_eq!(
            encode_frame(FILE_CHUNK, "a", b"xyz").unwrap().as_ref(),
            &[FILE_CHUNK, 1, b'a', b'x', b'y', b'z']
        );
        assert!(decode_frame(&[FILE_END, 1, 0xff]).is_some());
    }

    #[test]
    fn names_preserve_unicode_and_extensions() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("dir\\name.txt"), "dir_name.txt");
        assert_eq!(sanitize_file_name(" a<b>:\"c|?*.txt "), "a_b___c___.txt");
        assert_eq!(sanitize_file_name("line\nbreak.txt"), "line_break.txt");
        assert_eq!(sanitize_file_name(".."), "upload");
        assert_eq!(
            sanitize_file_name("résumé [final] & notes, v2.pdf"),
            "résumé [final] & notes, v2.pdf"
        );
        assert_eq!(
            sanitize_file_name(&format!("{}.xlsx", "绩".repeat(100))),
            format!("{}.xlsx", "绩".repeat(83))
        );
    }

    #[test]
    fn disposition_matches_utf16_fallback_and_rfc5987() {
        assert_eq!(
            content_disposition("notes.txt"),
            "attachment; filename=\"notes.txt\""
        );
        assert_eq!(
            content_disposition("a\"b\\c"),
            "attachment; filename=\"a\\\"b\\\\c\""
        );
        assert_eq!(
            content_disposition("😀.txt"),
            "attachment; filename=\"??.txt\"; filename*=UTF-8''%F0%9F%98%80.txt"
        );
        assert_eq!(
            content_disposition("x%20(').txt"),
            "attachment; filename=\"x%20(').txt\"; filename*=UTF-8''x%2520%28%27%29.txt"
        );
        assert_eq!(
            content_disposition("a\r\n.txt"),
            "attachment; filename=\"a??.txt\"; filename*=UTF-8''a%0D%0A.txt"
        );
    }

    #[tokio::test]
    async fn uploads_are_source_bound_and_keep_completed_files() {
        let home = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(home.path().to_owned(), Duration::from_secs(60));
        let (a, mut outbound) = RequestContext::attach(source("a"));
        let (b, _other_outbound) = RequestContext::attach(source("b"));
        let message = request("notes.txt", 3);
        let requesting = a.for_request(message.clone());
        transfers.begin(&requesting, &message).await.unwrap();
        requesting.finish_request();
        assert!(!transfers.receive(&b, encode_frame(FILE_CHUNK, "same", b"bad").unwrap()));
        receive(&transfers, &a, FILE_BEGIN, "same", &metadata());
        receive(&transfers, &a, FILE_CHUNK, "same", b"abc");
        receive(&transfers, &a, FILE_END, "same", b"");
        let response = response(&outbound.recv().await.unwrap().frame);
        let file = &response["payload"]["file"];
        let path = PathBuf::from(file["path"].as_str().unwrap());
        assert_eq!(fs::read(&path).await.unwrap(), b"abc");
        assert_eq!(
            response,
            json!({"type":"file.upload.response","payload":{
            "requestId":"same","file":{"type":"uploaded_file","id":file["id"],"fileName":"notes.txt",
            "mimeType":"text/plain","size":3,"path":path},"error":null}})
        );
        transfers.shutdown().await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn partial_uploads_clean_up_on_timeout_detach_and_replacement() {
        let home = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::new(home.path().to_owned(), Duration::from_secs(60));
        transfers.write_gate = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        let source = source("source");
        let source_cancel = source.cancel.clone();
        let (context, _outbound) = RequestContext::attach(source);
        let message = request("old.txt", 3);
        let requesting = context.for_request(message.clone());
        transfers.begin(&requesting, &message).await.unwrap();
        receive(&transfers, &context, FILE_BEGIN, "same", &metadata());
        let old = slot(&transfers, &context, "same");
        let old_directory = home.path().join("uploads").join(&old.id);
        wait_for_file(&old_directory.join("old.txt")).await;
        let message = request("new.txt", 3);
        transfers
            .begin(&context.for_request(message.clone()), &message)
            .await
            .unwrap();
        receive(&transfers, &context, FILE_BEGIN, "same", &metadata());
        let replacement = slot(&transfers, &context, "same");
        assert_ne!(replacement.id, old.id);
        let replacement_directory = home.path().join("uploads").join(&replacement.id);
        wait_for_file(&replacement_directory.join("new.txt")).await;
        replacement.deadline.send_replace(Instant::now());
        join_tasks(&transfers).await;
        assert!(!old_directory.exists());
        assert!(!replacement_directory.exists());
        assert!(transfers.registry.lock().unwrap().uploads.is_empty());
        transfers
            .begin(&context.for_request(message.clone()), &message)
            .await
            .unwrap();
        receive(&transfers, &context, FILE_BEGIN, "same", &metadata());
        let detached = home
            .path()
            .join("uploads")
            .join(&slot(&transfers, &context, "same").id);
        wait_for_file(&detached.join("new.txt")).await;
        receive(&transfers, &context, FILE_CHUNK, "same", b"ab");
        source_cancel.cancel();
        join_tasks(&transfers).await;
        assert!(!detached.exists());
        assert!(transfers.registry.lock().unwrap().usage.is_empty());
        transfers.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn chunk_boundaries_reject_out_of_order_and_oversize_data_and_remove_partial_data() {
        let home = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(home.path().to_owned(), Duration::from_secs(60));
        let (source, mut outbound) = RequestContext::attach(source("source"));
        for (started, opcode, bytes, expected) in [
            (false, FILE_CHUNK, b"x".as_slice(), "out of order"),
            (false, FILE_END, b"".as_slice(), "out of order"),
            (true, FILE_END, b"".as_slice(), "before reaching"),
            (true, FILE_CHUNK, b"cd".as_slice(), "exceeded"),
        ] {
            begin_request(&transfers, &source, "id", 3).await;
            let directory = home
                .path()
                .join("uploads")
                .join(&slot(&transfers, &source, "id").id);
            if started {
                receive(&transfers, &source, FILE_BEGIN, "id", &metadata());
                wait_for_file(&directory.join("test.txt")).await;
                receive(&transfers, &source, FILE_CHUNK, "id", b"ab");
            }
            receive(&transfers, &source, opcode, "id", bytes);
            let response = response(&outbound.recv().await.unwrap().frame);
            assert!(
                response["payload"]["error"]
                    .as_str()
                    .unwrap()
                    .contains(expected)
            );
            assert!(response["payload"]["file"].is_null());
            assert!(!directory.exists());
            join_tasks(&transfers).await;
        }
        transfers.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stalled_disk_preserves_chunk_order_without_blocking_other_uploads() {
        let home = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::new(home.path().to_owned(), Duration::from_secs(60));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        transfers.write_gate = Some(gate.clone());
        let (source, mut outbound) = RequestContext::attach(source("source"));
        begin_request(&transfers, &source, "slow", 3).await;
        receive(&transfers, &source, FILE_BEGIN, "slow", &metadata());
        let directory = home
            .path()
            .join("uploads")
            .join(&slot(&transfers, &source, "slow").id);
        wait_for_file(&directory.join("test.txt")).await;
        for bytes in [b"b", b"a", b"c"] {
            receive(&transfers, &source, FILE_CHUNK, "slow", bytes);
        }
        receive(&transfers, &source, FILE_END, "slow", b"");
        begin_request(&transfers, &source, "fast", 0).await;
        receive(&transfers, &source, FILE_BEGIN, "fast", &metadata());
        receive(&transfers, &source, FILE_END, "fast", b"");
        let frame = tokio::time::timeout(Duration::from_secs(5), outbound.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response(&frame.frame)["payload"]["requestId"], "fast");
        assert_eq!(fs::read(directory.join("test.txt")).await.unwrap(), b"");
        gate.add_permits(3);
        let frame = tokio::time::timeout(Duration::from_secs(5), outbound.recv())
            .await
            .unwrap()
            .unwrap();
        let response = response(&frame.frame);
        assert_eq!(response["payload"]["requestId"], "slow");
        let path = response["payload"]["file"]["path"].as_str().unwrap();
        assert_eq!(fs::read(path).await.unwrap(), b"bac");
        transfers.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fragmented_upload_cannot_overrun_its_queue() {
        let home = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::new(home.path().to_owned(), Duration::from_secs(60));
        transfers.write_gate = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        let (source, mut outbound) = RequestContext::attach(source("source"));
        begin_request(&transfers, &source, "slow", MAX_UPLOAD_BYTES).await;
        receive(&transfers, &source, FILE_BEGIN, "slow", &metadata());
        let directory = home
            .path()
            .join("uploads")
            .join(&slot(&transfers, &source, "slow").id);
        wait_for_file(&directory.join("test.txt")).await;
        for _ in 0..UPLOAD_QUEUE + 2 {
            receive(&transfers, &source, FILE_CHUNK, "slow", b"x");
        }
        receive(&transfers, &source, FILE_END, "slow", b"");
        let frame = tokio::time::timeout(Duration::from_secs(5), outbound.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response(&frame.frame)["payload"]["error"],
            "Upload transfer buffer is full"
        );
        assert!(!directory.exists());
        transfers.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quotas_limit_declared_bytes_and_count_per_source_and_daemon() {
        let home = tempfile::tempdir().unwrap();
        for (plan, probe, probe_bytes, error) in [
            (vec![vec![]], 0, MAX_UPLOAD_BYTES + 1, "FILE_TOO_LARGE"),
            (vec![vec![0; SOURCE_UPLOADS]], 0, 0, "connection"),
            (vec![vec![MAX_UPLOAD_BYTES; 2]], 0, 1, "connection"),
            (
                vec![vec![0; SOURCE_UPLOADS]; DAEMON_UPLOADS / SOURCE_UPLOADS],
                4,
                0,
                "daemon",
            ),
            (
                vec![vec![MAX_UPLOAD_BYTES; 2]; 5],
                5,
                DAEMON_UPLOAD_BYTES - 10 * MAX_UPLOAD_BYTES + 1,
                "daemon",
            ),
        ] {
            let transfers = Transfers::new(home.path().into(), Duration::from_secs(60));
            let mut sources: Vec<_> = (0..plan.len().max(probe + 1))
                .map(|i| RequestContext::attach(source(&i.to_string())))
                .collect();
            for ((context, _), sizes) in sources.iter().zip(&plan) {
                for (i, size) in sizes.iter().enumerate() {
                    let id = i.to_string();
                    begin_request(&transfers, context, &id, *size).await;
                    assert!(
                        context
                            .validate_binary(&encode_frame(FILE_END, &id, b"").unwrap(), true)
                            .is_ok()
                    );
                    assert!(
                        transfers
                            .registry
                            .lock()
                            .unwrap()
                            .slot(context.source_id(), &id)
                            .is_some()
                    );
                }
            }
            let (context, output) = &mut sources[probe];
            begin_request(&transfers, context, "rejected", probe_bytes).await;
            let frame = tokio::time::timeout(Duration::from_secs(5), output.recv())
                .await
                .unwrap()
                .unwrap();
            let response = response(&frame.frame);
            assert_eq!(response["type"], "rpc_error");
            if error == "FILE_TOO_LARGE" {
                assert_eq!(response["payload"]["code"], error);
                assert_eq!(response["payload"]["error"], "test.txt");
            } else {
                assert_eq!(response["payload"]["code"], "UPLOAD_LIMIT_EXCEEDED");
                assert!(
                    response["payload"]["error"]
                        .as_str()
                        .unwrap()
                        .contains(error)
                );
            }
            let frame = encode_frame(FILE_CHUNK, "rejected", b"already queued").unwrap();
            assert!(context.validate_binary(&frame, true).is_ok());
            assert!(transfers.receive(context, frame));
            assert!(
                transfers
                    .registry
                    .lock()
                    .unwrap()
                    .slot(context.source_id(), "rejected")
                    .is_none()
            );
            transfers.shutdown().await.unwrap();
            assert!(transfers.registry.lock().unwrap().usage.is_empty());
        }
        assert!(!home.path().join("uploads").exists());
    }

    #[tokio::test]
    async fn retention_protects_live_and_archived_references_until_the_last_owner_is_deleted() {
        let home = tempfile::tempdir().unwrap();
        let directory = home
            .path()
            .join("uploads")
            .join("upload_26abc008-50d2-401c-8bca-91f1bcfd7530");
        fs::create_dir_all(&directory).await.unwrap();
        let path = directory.join("test.txt");
        fs::write(&path, b"data").await.unwrap();
        let marker = upload_marker(
            home.path(),
            directory.file_name().unwrap().to_str().unwrap(),
        );
        write_upload_marker(&marker, &UploadReferences::default())
            .await
            .unwrap();
        retain_upload(&directory, &marker, "archived", true)
            .await
            .unwrap();
        retain_upload(&directory, &marker, "another", true)
            .await
            .unwrap();
        let now = SystemTime::now();
        let later = now + UPLOAD_RETENTION + Duration::from_secs(60);
        reclaim_upload(&directory, &marker, later, true, |agent| async move {
            Ok(agent == "archived")
        })
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"data");
        reclaim_upload(&directory, &marker, now, false, |_| async { Ok(false) })
            .await
            .unwrap();
        assert!(!directory.exists());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn retention_ages_orphans_and_fails_closed_on_unknown_legacy_references() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("uploads");
        let orphan = path.join("upload_26abc008-50d2-401c-8bca-91f1bcfd7530");
        let legacy = path.join("upload_0760cc2e-5b71-42a0-a26d-e95e4ba2cb21");
        let orphan_marker =
            upload_marker(home.path(), orphan.file_name().unwrap().to_str().unwrap());
        let legacy_marker =
            upload_marker(home.path(), legacy.file_name().unwrap().to_str().unwrap());
        fs::create_dir_all(&orphan).await.unwrap();
        fs::create_dir_all(&legacy).await.unwrap();
        write_upload_marker(&orphan_marker, &UploadReferences::default())
            .await
            .unwrap();
        let now = SystemTime::now();
        let later = now + UPLOAD_RETENTION + Duration::from_secs(60);
        reclaim_upload(&orphan, &orphan_marker, now, true, |_| async { Ok(false) })
            .await
            .unwrap();
        assert!(orphan.exists());
        reclaim_upload(&orphan, &orphan_marker, later, true, |_| async {
            Ok(false)
        })
        .await
        .unwrap();
        assert!(!orphan.exists());
        reclaim_upload(&legacy, &legacy_marker, later, true, |_| async {
            Ok(false)
        })
        .await
        .unwrap();
        assert!(legacy.exists());
        fs::write(&legacy_marker, b"corrupt").await.unwrap();
        assert!(
            reclaim_upload(&legacy, &legacy_marker, later, false, |_| async {
                Ok(false)
            })
            .await
            .is_err()
        );
        assert!(legacy.exists());
        fs::remove_file(&legacy_marker).await.unwrap();
        reclaim_upload(&legacy, &legacy_marker, later, false, |_| async {
            Ok(false)
        })
        .await
        .unwrap();
        assert!(!legacy.exists());
    }

    #[test]
    fn only_managed_upload_paths_in_prompt_are_pinned() {
        let home = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(home.path().into(), Duration::from_secs(60));
        let attachment = "upload_26abc008-50d2-401c-8bca-91f1bcfd7530";
        let pasted = "upload_0760cc2e-5b71-42a0-a26d-e95e4ba2cb21";
        let prompt = json!([
            {"type":"uploaded_file","path":home.path().join("uploads").join(attachment).join("a.txt")},
            {"type":"text","text":format!("Read {} next", home.path().join("uploads").join(pasted).join("b.txt").display())},
            {"type":"text","text":format!("Ignore {}", home.path().join("elsewhere").join(attachment).display())},
            {"type":"text","text":format!("Ignore {}", home.path().join("uploads").join("upload_760cc2e-5b71-42a0-a26d-e95e4ba2cb21").join("c.txt").display())},
        ]);
        assert_eq!(
            transfers.referenced_uploads(&prompt),
            HashMap::from([(attachment, true), (pasted, false)])
        );
    }

    #[tokio::test]
    async fn download_tokens_are_single_use_expire_and_read_current_size() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("report.txt");
        fs::write(&path, b"old").await.unwrap();
        let downloads = Downloads::new(Duration::from_secs(60));
        let token = downloads
            .issue(
                path.clone(),
                "report.txt".into(),
                "report.txt".into(),
                "text/plain".into(),
                3,
            )
            .await
            .unwrap();
        fs::write(&path, b"current").await.unwrap();
        let download = downloads.open(&token).await.unwrap().unwrap();
        assert_eq!(download.size, 7);
        assert_eq!(download.mime_type, "text/plain");
        assert_eq!(download.file_name, "report.txt");
        assert!(downloads.open(&token).await.unwrap().is_none());
        let expired = Downloads::new(Duration::ZERO);
        let token = expired
            .issue(
                path.clone(),
                "report.txt".into(),
                "report.txt".into(),
                "text/plain".into(),
                7,
            )
            .await
            .unwrap();
        assert!(expired.open(&token).await.unwrap().is_none());
        let token = downloads
            .issue(
                path.clone(),
                "report.txt".into(),
                "report.txt".into(),
                "text/plain".into(),
                7,
            )
            .await
            .unwrap();
        fs::remove_file(&path).await.unwrap();
        fs::create_dir(&path).await.unwrap();
        assert!(downloads.open(&token).await.is_err());
        assert!(downloads.open(&token).await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn downloads_reject_leaf_and_parent_symlink_replacement() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join("parent");
        fs::create_dir(&parent).await.unwrap();
        let path = parent.join("data");
        let other = home.path().join("other");
        fs::write(&path, b"safe").await.unwrap();
        fs::write(&other, b"private").await.unwrap();
        let downloads = Downloads::new(Duration::from_secs(60));
        let token = downloads
            .issue(
                path.clone(),
                "data".into(),
                "data".into(),
                "text/plain".into(),
                4,
            )
            .await
            .unwrap();
        fs::remove_file(&path).await.unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(downloads.open(&token).await.is_err());
        fs::remove_file(&path).await.unwrap();
        fs::write(&path, b"safe").await.unwrap();
        let token = downloads
            .issue(
                path.clone(),
                "data".into(),
                "data".into(),
                "text/plain".into(),
                4,
            )
            .await
            .unwrap();
        let moved = home.path().join("moved");
        fs::rename(&parent, &moved).await.unwrap();
        std::os::unix::fs::symlink(&moved, &parent).unwrap();
        assert!(downloads.open(&token).await.is_err());
    }
}
