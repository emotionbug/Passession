//! Save / share / open a downloaded copy (Flutter `native_file_actions.dart saveRemoteFile`).
//!
//! Desktop "save" asks for a destination first (cancel aborts). The file downloads into a unique
//! staging file (`<temp>/paseo-files/transfer-*/<name>`) with a cancellable progress card; a
//! failed or cancelled transfer never replaces a saved file and its staging directory is removed.
//! An unsaved modified buffer is written as the current bytes instead of downloading. Mobile
//! always shares.
//! In the browser the completed bytes go directly to the download API.

#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::{CoreHandle, RequestId};
#[cfg(not(target_arch = "wasm32"))]
use serde_json::Value;
use serde_json::json;

use super::path::remote_basename;
#[cfg(not(target_arch = "wasm32"))]
use super::path::safe_local_name;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferMode {
    Save,
    Share,
    #[cfg(not(target_arch = "wasm32"))]
    Open,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub enum TransferStage {
    /// Waiting for the platform save dialog.
    Choosing,
    Running,
}

/// One transfer, owned by the panel that started it (its progress card).
pub struct Transfer {
    pub id: u64,
    pub owner: String,
    pub host_id: String,
    pub cwd: String,
    pub path: String,
    pub name: String,
    #[cfg(not(target_arch = "wasm32"))]
    pub mode: TransferMode,
    pub mime: Option<String>,
    pub current_bytes: Option<Arc<Vec<u8>>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub destination: Option<PathBuf>,
    #[cfg(not(target_arch = "wasm32"))]
    pub staging: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    pub stage: TransferStage,
    #[cfg(target_arch = "wasm32")]
    pub download: Option<std::sync::mpsc::Receiver<Arc<Vec<u8>>>>,
    pub received: u64,
    pub total: u64,
    pub request: Option<RequestId>,
    #[cfg(not(target_arch = "wasm32"))]
    pub platform_request: Option<u64>,
    #[cfg(not(target_arch = "wasm32"))]
    pub cancelled: bool,
}

/// Progress posted by a running transfer.
pub struct Progress {
    pub id: u64,
    pub received: u64,
    pub total: u64,
}

impl Transfer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        owner: &str,
        host_id: &str,
        cwd: &str,
        path: &str,
        #[cfg(not(target_arch = "wasm32"))] mode: TransferMode,
        #[cfg(target_arch = "wasm32")] _mode: TransferMode,
        mime: Option<String>,
        current_bytes: Option<Arc<Vec<u8>>>,
    ) -> Transfer {
        let name = remote_basename(path, Some(cwd));
        #[cfg(not(target_arch = "wasm32"))]
        let staging = staging_path(&name);
        Transfer {
            id,
            owner: owner.to_owned(),
            host_id: host_id.to_owned(),
            cwd: cwd.to_owned(),
            path: path.to_owned(),
            name,
            #[cfg(not(target_arch = "wasm32"))]
            mode,
            mime,
            current_bytes,
            #[cfg(not(target_arch = "wasm32"))]
            destination: None,
            #[cfg(not(target_arch = "wasm32"))]
            staging,
            #[cfg(not(target_arch = "wasm32"))]
            stage: TransferStage::Running,
            #[cfg(target_arch = "wasm32")]
            download: None,
            received: 0,
            total: 0,
            request: None,
            #[cfg(not(target_arch = "wasm32"))]
            platform_request: None,
            #[cfg(not(target_arch = "wasm32"))]
            cancelled: false,
        }
    }

    /// Starts the download (or the current-bytes write) as one core operation; progress
    /// arrives through `progress`. The response is `{"path", "mime", "saved"}`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn run(&mut self, core: &CoreHandle, progress: ToUISender<Progress>) {
        self.stage = TransferStage::Running;
        let id = self.id;
        let (cwd, path) = (self.cwd.clone(), self.path.clone());
        let staging = self.staging.clone();
        let destination = self.destination.clone();
        let current = self.current_bytes.clone();
        let mime = self.mime.clone();
        self.request = Some(core.operation(&self.host_id, move |client| async move {
            if let Some(parent) = staging.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let metadata = match current {
                Some(bytes) => {
                    std::fs::write(&staging, bytes.as_slice())?;
                    let size = bytes.len() as u64;
                    let _ = progress.send(Progress {
                        id,
                        received: size,
                        total: size,
                    });
                    json!({})
                }
                None => {
                    let sender = progress.clone();
                    super::service::download_into(
                        &client,
                        &cwd,
                        &path,
                        &staging,
                        move |received, total| {
                            let _ = sender.send(Progress {
                                id,
                                received,
                                total,
                            });
                        },
                    )
                    .await?
                }
            };
            let mime = mime
                .or_else(|| {
                    metadata
                        .get("mime")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            if let Some(destination) = destination {
                // `XFile.saveTo` then delete the staging directory.
                std::fs::copy(&staging, &destination)?;
                remove_staging(&staging);
                return Ok(
                    json!({ "path": destination.to_string_lossy(), "mime": mime, "saved": true }),
                );
            }
            Ok(json!({ "path": staging.to_string_lossy(), "mime": mime, "saved": false }))
        }));
    }

    #[cfg(target_arch = "wasm32")]
    pub fn run(&mut self, core: &CoreHandle, progress: ToUISender<Progress>) {
        let id = self.id;
        let (cwd, path) = (self.cwd.clone(), self.path.clone());
        let current = self.current_bytes.take();
        let mime = self.mime.clone();
        let (send, recv) = std::sync::mpsc::channel();
        self.download = Some(recv);
        self.request = Some(core.operation(&self.host_id, move |client| async move {
            let report = |received, total| {
                let _ = progress.send(Progress {
                    id,
                    received,
                    total,
                });
            };
            let (bytes, mime) = match current {
                Some(bytes) => {
                    let size = bytes.len() as u64;
                    report(size, size);
                    (bytes, mime)
                }
                None => {
                    let mut bytes = Vec::new();
                    let metadata =
                        super::service::download_to(&client, &cwd, &path, &mut bytes, report)
                            .await?;
                    let mime = mime.or_else(|| metadata["mime"].as_str().map(str::to_owned));
                    (Arc::new(bytes), mime)
                }
            };
            send.send(bytes)?;
            Ok(json!({ "mime": mime }))
        }));
    }

    pub fn is_running(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return matches!(self.stage, TransferStage::Running);
        #[cfg(target_arch = "wasm32")]
        true
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn staging_path(name: &str) -> PathBuf {
    paseo_gui_platform::paths::app_paths()
        .temp_dir
        .join("paseo-files")
        .join(format!("transfer-{}", uuid::Uuid::new_v4().simple()))
        .join(safe_local_name(name))
}

/// Removes a staging file's `transfer-*` directory.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_staging(staging: &Path) {
    if let Some(directory) = staging.parent() {
        let _ = std::fs::remove_dir_all(directory);
    }
}

/// Removes staging off the GUI thread (cancel/failure cleanup).
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_staging_later(staging: PathBuf) {
    std::thread::Builder::new()
        .name("paseo-files-cleanup".into())
        .spawn(move || remove_staging(&staging))
        .ok();
}
