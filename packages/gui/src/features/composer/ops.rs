//! Async bodies run through `CoreHandle::operation` on the core runtime (never the GUI
//! thread). They read retained image blobs / picked files, encode them for the wire and call
//! the SDK. Results always come back as an envelope so the daemon's error payload (message
//! receipts, upload errors) survives: `{"ok": true, "payload": …}` or
//! `{"ok": false, "error": {"code", "message", "details", "outcomeUnknown"}}`.

use super::blobs::read_blob;
use base64::Engine;
use paseo_client::{ClientError, DaemonClient, RequestOptions};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// SDK codes whose outcome is unknown once the request was submitted.
fn outcome_unknown(error: &ClientError) -> bool {
    if let Some(flag) = error.details.as_ref().and_then(|d| d.get("outcomeUnknown")).and_then(Value::as_bool) {
        return flag;
    }
    matches!(
        error.code.as_str(),
        "DAEMON_REQUEST_TIMEOUT" | "DAEMON_CONNECTION_LOST" | "DAEMON_REQUEST_CANCELLED"
    ) && !error.message.contains("before submission")
}

pub fn envelope_error(error: anyhow::Error) -> Value {
    match error.downcast::<ClientError>() {
        Ok(error) => {
            let unknown = outcome_unknown(&error);
            json!({
                "ok": false,
                "error": {
                    "code": error.code,
                    "message": error.message,
                    "details": error.details.unwrap_or(Value::Null),
                    "outcomeUnknown": unknown,
                }
            })
        }
        Err(error) => json!({
            "ok": false,
            "error": {"code": "CLIENT_ERROR", "message": error.to_string(), "details": null, "outcomeUnknown": false}
        }),
    }
}

/// A local failure before anything reached the daemon (definite: nothing was sent).
fn local_error(message: String) -> Value {
    json!({"ok": false, "error": {"code": "LOCAL_ERROR", "message": message, "details": null, "outcomeUnknown": false}})
}

/// Wire images: retained blobs are read and base64-encoded; inline legacy images pass as-is.
async fn wire_images(images: Vec<Value>, blob_dir: PathBuf) -> Result<Vec<Value>, String> {
    tokio::task::spawn_blocking(move || {
        images
            .into_iter()
            .map(|image| {
                let mime = image.get("mimeType").and_then(Value::as_str).unwrap_or("image/png").to_owned();
                if let Some(id) = image.get("blobId").and_then(Value::as_str) {
                    let bytes = read_blob(&blob_dir, id)?;
                    Ok(json!({"data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": mime}))
                } else {
                    Ok(json!({
                        "data": image.get("data").cloned().unwrap_or(Value::Null),
                        "mimeType": mime,
                    }))
                }
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run one RPC whose request may carry composer images (send_agent_message, agent.create;
/// workspace.create has none). `images` replace the request's `images` field once encoded.
/// `timeout` None = no deadline (creation lifecycle).
pub async fn rpc_with_images(
    client: Arc<DaemonClient>,
    request: String,
    response: String,
    mut fields: Value,
    images: Option<Vec<Value>>,
    blob_dir: PathBuf,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<Value> {
    if let Some(images) = images {
        let wire = match wire_images(images, blob_dir).await {
            Ok(wire) => wire,
            Err(message) => return Ok(local_error(message)),
        };
        if let Some(map) = fields.as_object_mut() {
            map.insert("images".into(), Value::Array(wire));
        }
    }
    let options = RequestOptions {
        timeout: timeout.unwrap_or(std::time::Duration::ZERO),
        ..RequestOptions::default()
    };
    match client.rpc(&request, &response, fields, options).await {
        Ok(payload) => {
            if payload.get("ok") == Some(&Value::Bool(false)) {
                let message = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("The daemon rejected this operation")
                    .to_owned();
                return Ok(json!({"ok": false, "error": {"code": "DAEMON_RPC_ERROR", "message": message, "details": payload, "outcomeUnknown": false}}));
            }
            Ok(json!({"ok": true, "payload": payload}))
        }
        Err(error) => Ok(envelope_error(error)),
    }
}

/// actions.ts uploadFileAttachments: upload each file in order; the uploaded files attach
/// together once all succeed. Files are read here, off the GUI thread.
pub async fn upload_files(client: Arc<DaemonClient>, files: Vec<(PathBuf, String, String)>) -> anyhow::Result<Value> {
    let mut uploaded = Vec::with_capacity(files.len());
    for (path, name, mime) in files {
        let read_path = path.clone();
        let read = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&read_path)?;
            let modified = std::fs::metadata(&read_path)
                .and_then(|m| m.modified())
                .ok()
                .map(chrono_like_rfc3339);
            Ok::<_, std::io::Error>((bytes, modified))
        })
        .await;
        let (bytes, modified) = match read {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Ok(local_error(format!("{name}: {error}"))),
            Err(error) => return Ok(local_error(error.to_string())),
        };
        let modified = modified.unwrap_or_else(|| chrono_like_rfc3339(std::time::SystemTime::now()));
        // Uploads have no deadline (Flutter streamed until the host answered or the
        // connection dropped); cancel_request drops this future.
        let options = RequestOptions { timeout: std::time::Duration::ZERO, ..RequestOptions::default() };
        match client.upload_file(&name, &mime, &modified, &bytes, options).await {
            Ok(response) => {
                let file = response.get("file").cloned().unwrap_or(Value::Null);
                if response.get("error").is_some_and(|e| !e.is_null()) || file.is_null() {
                    let message = response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Upload was not acknowledged")
                        .to_owned();
                    return Ok(local_error(message));
                }
                uploaded.push(file);
            }
            Err(error) => return Ok(envelope_error(error)),
        }
    }
    Ok(json!({"ok": true, "payload": {"files": uploaded}}))
}

/// RFC 3339 UTC timestamp without pulling a date crate into the composer.
fn chrono_like_rfc3339(time: std::time::SystemTime) -> String {
    let secs = time.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
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
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.000Z")
}

pub fn now_rfc3339() -> String {
    chrono_like_rfc3339(std::time::SystemTime::now())
}

/// Decode an operation envelope: Ok(payload) or Err(error object).
pub fn open_envelope(value: &Value) -> Result<Value, Value> {
    if value.get("ok") == Some(&Value::Bool(true)) {
        Ok(value.get("payload").cloned().unwrap_or(Value::Null))
    } else {
        Err(value.get("error").cloned().unwrap_or_else(|| json!({"message": "Operation failed"})))
    }
}
