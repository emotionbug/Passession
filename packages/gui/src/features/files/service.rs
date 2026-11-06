//! Daemon IO for files (Flutter `remote_file_service.dart` + `DaemonFileHost`). Everything runs
//! through the core: JSON RPCs with `CoreHandle::request`, observations with
//! `CoreHandle::observe`, binary transfers inside `CoreHandle::operation` closures using the SDK
//! (`paseo_client::DaemonClient::read_file` / `upload_file`). Results return as core `Response`
//! events; bytes never travel through JSON — an operation parks them in the [`Stash`] under a
//! ticket and returns only `{"ticket": n}`.

use crate::features::files::handle::FileContents;
use paseo_gui_core::{CoreHandle, RequestId};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Preview ceiling for assets read for display (Markdown images, HTML assets): 32 MiB.
pub const PREVIEW_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Editor reads are unbounded in the original (`maxBytes: null`); the SDK needs a ceiling, so a
/// value far above every presentation budget is used.
pub fn unbounded_read() -> usize {
    usize::try_from(1u64 << 40).unwrap_or(usize::MAX)
}

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

pub fn next_ticket() -> u64 {
    NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
}

/// Binary payloads parked by operations until the GUI thread takes them with the `Response`.
#[derive(Clone, Default)]
pub struct Stash(Arc<Mutex<HashMap<u64, FileContents>>>);

impl Stash {
    fn put(&self, ticket: u64, contents: FileContents) {
        self.0.lock().unwrap().insert(ticket, contents);
    }

    pub fn take(&self, ticket: u64) -> Option<FileContents> {
        self.0.lock().unwrap().remove(&ticket)
    }

    /// Drops a payload whose owner went away (the response was a failure or arrived late).
    pub fn discard(&self, ticket: u64) {
        self.0.lock().unwrap().remove(&ticket);
    }
}

fn contents_from_download(file: paseo_client::DownloadedFile) -> FileContents {
    let mime = file.metadata.mime.clone();
    let text = matches!(file.metadata.encoding, paseo_protocol::binary_frames::FileEncoding::Utf8);
    let kind = if mime.starts_with("image/") {
        "image"
    } else if text {
        "text"
    } else {
        "binary"
    };
    FileContents {
        size: file.metadata.size,
        modified_at: file.metadata.modified_at.clone(),
        revision: file.metadata.revision.clone(),
        kind: kind.to_owned(),
        mime_type: mime,
        bytes: Arc::new(file.bytes.to_vec()),
    }
}

/// Reads a whole file through the framed binary transfer (legacy JSON responses accepted by the
/// SDK). The response is `{"ticket": n}`; take the contents from the stash.
pub fn read(
    core: &CoreHandle,
    stash: &Stash,
    host_id: &str,
    cwd: &str,
    path: &str,
    max_bytes: usize,
) -> (RequestId, u64) {
    let ticket = next_ticket();
    let stash = stash.clone();
    let cwd = cwd.to_owned();
    let path = path.to_owned();
    let request = core.operation(host_id, move |client| async move {
        let options = paseo_client::RequestOptions {
            timeout: Duration::from_secs(300),
            ..Default::default()
        };
        let file = client.read_file(&cwd, &path, max_bytes, options).await?;
        stash.put(ticket, contents_from_download(file));
        Ok(json!({ "ticket": ticket }))
    });
    (request, ticket)
}

/// `file_explorer_request` in list mode.
pub fn list_directory(core: &CoreHandle, host_id: &str, cwd: &str, path: &str) -> RequestId {
    core.request(
        host_id,
        json!({"type": "file_explorer_request", "cwd": cwd, "path": path, "mode": "list"}),
        "file_explorer_response",
    )
}

/// `fs.entry.<action>.request` with `cwd` and the action's fields.
pub fn entry_request(
    core: &CoreHandle,
    host_id: &str,
    cwd: &str,
    action: &str,
    mut fields: Value,
) -> RequestId {
    fields["type"] = Value::String(format!("fs.entry.{action}.request"));
    fields["cwd"] = Value::String(cwd.to_owned());
    core.request(host_id, fields, &format!("fs.entry.{action}.response"))
}

/// One `fs.file.write.request` attempt over an expected version.
pub fn write(
    core: &CoreHandle,
    host_id: &str,
    cwd: &str,
    path: &str,
    content: String,
    expected_modified_at: &str,
    expected_revision: Option<&str>,
) -> RequestId {
    let mut message = json!({
        "type": "fs.file.write.request",
        "cwd": cwd,
        "path": path,
        "content": content,
        "expectedModifiedAt": expected_modified_at,
    });
    if let Some(revision) = expected_revision {
        message["expectedRevision"] = Value::String(revision.to_owned());
    }
    core.request(host_id, message, "fs.file.write.response")
}

/// The `fs.file.write.response` result object from a response payload or a rejected request's
/// details (original `DaemonException` with `payload.result.status`).
pub fn write_result(payload: &Value) -> Option<Value> {
    let result = payload.get("result")?;
    result.get("status").and_then(Value::as_str).map(|_| result.clone())
}

/// `fs.file.subscribe.request` as an owned, reconnect-restoring core observation.
pub fn observe_file(core: &CoreHandle, host_id: &str, cwd: &str, path: &str) -> RequestId {
    core.observe(
        host_id,
        json!({"type": "fs.file.subscribe.request", "cwd": cwd, "path": path}),
        "fs.file.subscribe.response",
    )
}

/// The domain error of a JSON response payload (`error`, `success:false`), or `None`.
pub fn payload_error(payload: &Value, request_type: &str) -> Option<String> {
    paseo_gui_core::response_error(payload, request_type).map(|error| error.message)
}

/// Downloads `path` into `destination` through the framed transfer (legacy JSON answers
/// accepted), reporting `(received, total)` progress. Returns the file metadata
/// `{mime, size, modifiedAt, revision}`. A failed transfer removes the partial file; a
/// cancelled one (future dropped) leaves it for the caller's staging cleanup.
pub async fn download_into(
    client: &paseo_client::DaemonClient,
    cwd: &str,
    path: &str,
    destination: &std::path::Path,
    mut progress: impl FnMut(u64, u64),
) -> anyhow::Result<Value> {
    use paseo_protocol::binary_frames::{FileTransferFrame, decode_file_transfer_frame};
    let id = uuid::Uuid::new_v4().to_string();
    let mut binary = client.subscribe_binary();
    let cancel = paseo_client::RequestOptions::default().cancel;
    let request = client.request_cancellable(
        json!({
            "type": "file_explorer_request",
            "cwd": cwd,
            "path": path,
            "mode": "file",
            "acceptBinary": true,
            "requestId": id,
        }),
        "file_explorer_response",
        Duration::ZERO,
        cancel,
    );
    tokio::pin!(request);
    let mut file = std::fs::File::create(destination)?;
    let outcome: anyhow::Result<Value> = async {
        let mut metadata: Option<Value> = None;
        let mut total = 0u64;
        let mut received = 0u64;
        loop {
            tokio::select! { biased;
                result = &mut request => {
                    // A legacy host answers with the whole file in JSON.
                    let payload = result?;
                    let entry = payload.get("file").cloned().unwrap_or(Value::Null);
                    let content = entry.get("content").and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("Host did not return file bytes"))?;
                    let bytes = if entry.get("encoding").and_then(Value::as_str) == Some("base64") {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(content)?
                    } else {
                        content.as_bytes().to_vec()
                    };
                    file.write_all(&bytes)?;
                    file.flush()?;
                    let size = bytes.len() as u64;
                    progress(size, size);
                    return Ok(json!({
                        "mime": entry.get("mimeType").cloned().unwrap_or(json!("application/octet-stream")),
                        "size": size,
                        "modifiedAt": entry.get("modifiedAt").cloned().unwrap_or(json!("")),
                        "revision": entry.get("revision").cloned().unwrap_or(Value::Null),
                    }));
                }
                frame = binary.recv() => {
                    let frame = frame.map_err(|_| anyhow::anyhow!("Connection lost during file download"))?;
                    if frame.first().is_none_or(|byte| !matches!(*byte, 0x10..=0x12)) {
                        continue;
                    }
                    match decode_file_transfer_frame(&frame)? {
                        FileTransferFrame::Begin { request_id, metadata: begin } if request_id == id => {
                            anyhow::ensure!(metadata.is_none(), "Duplicate file transfer header");
                            total = begin.size;
                            metadata = Some(json!({
                                "mime": begin.mime,
                                "size": begin.size,
                                "modifiedAt": begin.modified_at,
                                "revision": begin.revision,
                            }));
                            progress(0, total);
                        }
                        FileTransferFrame::Chunk { request_id, payload } if request_id == id => {
                            anyhow::ensure!(metadata.is_some(), "File data arrived before its header");
                            received += payload.len() as u64;
                            anyhow::ensure!(received <= total, "File transfer exceeded declared size");
                            file.write_all(payload)?;
                            progress(received, total);
                        }
                        FileTransferFrame::End { request_id } if request_id == id => {
                            let metadata = metadata.take()
                                .ok_or_else(|| anyhow::anyhow!("Incomplete file transfer ({received} bytes)"))?;
                            anyhow::ensure!(received == total, "Incomplete file transfer ({received} bytes)");
                            file.flush()?;
                            file.sync_all()?;
                            return Ok(metadata);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    .await;
    if outcome.is_err() {
        drop(file);
        let _ = std::fs::remove_file(destination);
    }
    outcome
}

/// Uploads bytes through the framed transfer (Flutter `RemoteFileService.upload`). The
/// response is the daemon's `file` object. Exported for ComposerDelivery attachment uploads.
pub fn upload_bytes(
    core: &CoreHandle,
    host_id: &str,
    file_name: &str,
    mime_type: &str,
    modified_at: Option<&str>,
    bytes: Arc<Vec<u8>>,
) -> RequestId {
    let file_name = file_name.to_owned();
    let mime_type = mime_type.to_owned();
    let modified_at = modified_at.map(str::to_owned).unwrap_or_else(now_iso8601);
    core.operation(host_id, move |client| async move {
        let options = paseo_client::RequestOptions {
            timeout: Duration::from_secs(600),
            ..Default::default()
        };
        let response = client
            .upload_file(&file_name, &mime_type, &modified_at, bytes.as_slice(), options)
            .await?;
        match response.get("file") {
            Some(file) if !file.is_null() && response.get("error").is_none_or(Value::is_null) => {
                Ok(file.clone())
            }
            _ => Err(anyhow::anyhow!(
                "{}",
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Upload was not acknowledged")
            )),
        }
    })
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SS.sssZ` (Dart `toUtc().toIso8601String()`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();
    let seconds = now.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_result_reads_the_result_object() {
        let payload = json!({"result": {"status": "conflict", "version": {"status": "missing"}}});
        assert_eq!(write_result(&payload).unwrap()["status"], "conflict");
        assert!(write_result(&json!({"error": "x"})).is_none());
    }

    #[test]
    fn iso_time_has_the_dart_shape() {
        let value = now_iso8601();
        assert_eq!(value.len(), 24);
        assert!(value.ends_with('Z'));
        assert_eq!(&value[4..5], "-");
        assert_eq!(&value[10..11], "T");
    }
}
