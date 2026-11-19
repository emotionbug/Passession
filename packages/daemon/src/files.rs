mod fs;
pub(crate) use fs::filesystem_space;
mod icons;
pub use icons::{extract_ico_png_frame, image_dimensions, project_icon};
pub(crate) mod observer;
mod picker;
mod transfer;

use crate::{state::AppState, transport::RequestContext};
use anyhow::{Result, anyhow, ensure};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::{Duration, SystemTime},
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, Notify, OnceCell},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
pub use transfer::FileDownload;

pub struct FileService {
    app: Weak<AppState>,
    transfers: OnceCell<transfer::Transfers>,
    downloads: OnceCell<transfer::Downloads>,
    observers: observer::Observers,
    writes: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
    retention_cancel: CancellationToken,
    retention_wake: Arc<Notify>,
    retention_task: StdMutex<Option<JoinHandle<()>>>,
}
impl FileService {
    pub fn new(app: Weak<AppState>) -> Self {
        Self {
            app,
            transfers: OnceCell::new(),
            downloads: OnceCell::new(),
            observers: observer::Observers::new(),
            writes: Mutex::new(HashMap::new()),
            retention_cancel: CancellationToken::new(),
            retention_wake: Arc::new(Notify::new()),
            retention_task: StdMutex::new(None),
        }
    }
    pub async fn initialize(&self) -> Result<()> {
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon is shutting down"))?;
        let ttl = Duration::from_millis(
            app.config.snapshot()["downloadTokenTtlMs"]
                .as_u64()
                .unwrap_or(60_000),
        );
        self.transfers
            .get_or_init(|| async {
                transfer::Transfers::new(app.home.clone(), Duration::from_secs(10 * 60))
            })
            .await;
        self.downloads
            .get_or_init(|| async { transfer::Downloads::new(ttl) })
            .await;
        Ok(())
    }
    pub(crate) fn start_upload_retention(&self) {
        let mut task = self.retention_task.lock().unwrap();
        if task.is_some() || self.retention_cancel.is_cancelled() {
            return;
        }
        let app = self.app.clone();
        let cancel = self.retention_cancel.clone();
        let wake = self.retention_wake.clone();
        *task = Some(tokio::spawn(async move {
            // The first tick sweeps at startup, then daily or after a deletion.
            let mut daily = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
            daily.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { biased;
                    _ = cancel.cancelled() => break,
                    _ = daily.tick() => {},
                    _ = wake.notified() => {},
                }
                let Some(app) = app.upgrade() else {
                    break;
                };
                if let Err(error) = app.files.sweep_uploads(&app).await {
                    tracing::warn!(%error, "Upload retention sweep failed");
                }
            }
        }));
    }

    pub(crate) fn upload_owner_deleted(&self) {
        self.retention_wake.notify_one();
    }

    pub(crate) fn upload_references<'a>(&self, prompt: &'a Value) -> HashMap<&'a str, bool> {
        self.transfers.get().map_or_else(HashMap::new, |transfers| {
            transfers.referenced_uploads(prompt)
        })
    }

    pub(crate) async fn retain_uploads(
        &self,
        agent: &str,
        uploads: HashMap<&str, bool>,
    ) -> Result<()> {
        let transfers = self
            .transfers
            .get()
            .ok_or_else(|| anyhow!("File service is not initialized"))?;
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon is shutting down"))?;
        let root = app.home.join("uploads");
        for (id, explicit) in uploads {
            let marker = transfer::upload_marker(&app.home, id);
            let lock = self.write_lock(marker.clone()).await;
            let _guard = lock.lock().await;
            ensure!(!transfers.is_receiving(id), "Upload is not complete: {id}");
            transfer::retain_upload(&root.join(id), &marker, agent, explicit).await?;
        }
        Ok(())
    }

    async fn sweep_uploads(&self, app: &AppState) -> Result<()> {
        let Some(transfers) = self.transfers.get() else {
            return Ok(());
        };
        let root = app.home.join("uploads");
        match tokio::fs::symlink_metadata(&root).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let mut entries = tokio::fs::read_dir(root).await?;
        let protect_legacy = app.agents.has_upload_owners().await?;
        let now = SystemTime::now();
        while !self.retention_cancel.is_cancelled() {
            let Some(entry) = entries.next_entry().await? else {
                break;
            };
            let name = entry.file_name();
            let Some(id) = name.to_str().filter(|id| transfer::is_upload_id(id)) else {
                continue;
            };
            let marker = transfer::upload_marker(&app.home, id);
            let lock = self.write_lock(marker.clone()).await;
            let guard = lock.lock().await;
            if !transfers.is_receiving(id)
                && let Err(error) = transfer::reclaim_upload(
                    &entry.path(),
                    &marker,
                    now,
                    protect_legacy,
                    |agent| async move { app.agents.upload_owner_exists(&agent).await },
                )
                .await
            {
                tracing::warn!(upload = id, %error, "Could not reclaim upload");
            }
            drop(guard);
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.observers.shutdown();
        self.retention_cancel.cancel();
        let retention_task = self.retention_task.lock().unwrap().take();
        let retention = match retention_task {
            Some(task) => task.await.map_err(anyhow::Error::from),
            None => Ok(()),
        };
        if let Some(transfers) = self.transfers.get() {
            transfers.shutdown().await?;
        }
        retention
    }
    pub fn handle_binary(&self, context: &RequestContext, bytes: Bytes) -> Result<bool> {
        ensure!(
            context.allows("workspace.write"),
            "File upload is not permitted"
        );
        Ok(self
            .transfers
            .get()
            .ok_or_else(|| anyhow!("File service is not initialized"))?
            .receive(context, bytes))
    }
    pub async fn open_download(&self, token: &str) -> Result<Option<FileDownload>> {
        self.downloads
            .get()
            .ok_or_else(|| anyhow!("File service is not initialized"))?
            .open(token)
            .await
    }
    async fn write_lock(&self, path: PathBuf) -> Arc<Mutex<()>> {
        #[cfg(windows)]
        let path = PathBuf::from(path.to_string_lossy().to_lowercase());
        let mut locks = self.writes.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&path).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(path, Arc::downgrade(&lock));
        lock
    }
    pub async fn handle(&self, context: &RequestContext, message: &Value) -> Result<bool> {
        let kind = message["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "file_explorer_request"
                | "directory_suggestions_request"
                | "fs.file.subscribe.request"
                | "fs.file.unsubscribe.request"
                | "fs.file.write.request"
                | "fs.entry.create.request"
                | "fs.entry.rename.request"
                | "fs.entry.duplicate.request"
                | "fs.entry.delete.request"
                | "file.upload.request"
                | "file_download_token_request"
        ) {
            return Ok(false);
        }
        let mutation = matches!(
            kind,
            "fs.file.write.request"
                | "fs.entry.create.request"
                | "fs.entry.rename.request"
                | "fs.entry.duplicate.request"
                | "fs.entry.delete.request"
                | "file.upload.request"
        );
        ensure!(
            context.allows(if mutation {
                "workspace.write"
            } else {
                "workspace.read"
            }),
            "File operation is not permitted"
        );
        ensure!(
            !context.cancellation().is_cancelled(),
            "Physical source is closed"
        );
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon is shutting down"))?;
        let home = app.user_home.clone();
        drop(app);
        // Source file messages carry their fields at the top level, unlike many
        // other session families. Do not reinterpret cwd as a workspace identity.
        let request = message;
        match kind {
            "file.upload.request" => {
                self.transfers
                    .get()
                    .ok_or_else(|| anyhow!("File service is not initialized"))?
                    .begin(context, request)
                    .await?;
            }
            "directory_suggestions_request" => {
                let result=picker::suggest(request,&home,context.cancellation()).await.unwrap_or_else(|error|json!({"directories":[],"entries":[],"error":error.to_string(),"requestId":request["requestId"]}));
                context
                    .send(json!({"type":"directory_suggestions_response","payload":result}))
                    .await?;
            }
            "fs.file.unsubscribe.request" => {
                context
                    .release_subscription(fs::field(request, "subscriptionId")?)
                    .await?;
                context.send(json!({"type":"fs.file.unsubscribe.response","payload":{"subscriptionId":request["subscriptionId"],"requestId":request["requestId"]}})).await?;
            }
            "fs.file.subscribe.request" => {
                self.subscribe(context, request, home).await?;
            }
            "fs.file.write.request" => {
                let (r, h) = (request.clone(), home.clone());
                let target = tokio::task::spawn_blocking(move || {
                    fs::scoped(fs::field(&r, "cwd")?, fs::field(&r, "path")?, &h)
                        .map(|s| s.resolved)
                })
                .await?;
                let result = match target {
                    Ok(path) => {
                        let lock = self.write_lock(path).await;
                        let cancel = context.cancellation();
                        let _guard = tokio::select! {biased;_=cancel.cancelled()=>return Err(anyhow!("Physical source is closed")),guard=lock.lock()=>guard};
                        let r = request.clone();
                        tokio::task::spawn_blocking(move || fs::write(&r, &home, &cancel)).await?
                    }
                    Err(error) => json!({"status":"error","error":error.to_string()}),
                };
                context.send(json!({"type":"fs.file.write.response","payload":{"result":result,"requestId":request["requestId"]}})).await?;
            }
            "fs.entry.create.request"
            | "fs.entry.rename.request"
            | "fs.entry.duplicate.request"
            | "fs.entry.delete.request" => {
                let r = request.clone();
                let cancel = context.cancellation();
                let result =
                    tokio::task::spawn_blocking(move || fs::mutate(&r, &home, &cancel)).await?;
                let mut payload = json!({"cwd":request["cwd"],"requestId":request["requestId"],"success":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string)});
                if kind == "fs.entry.create.request" {
                    payload["parentPath"] = request["parentPath"].clone();
                    payload["path"] = result.ok().map(Value::String).unwrap_or(Value::Null);
                } else {
                    payload["path"] = request["path"].clone();
                    if kind == "fs.entry.rename.request" {
                        payload["renamedPath"] =
                            result.ok().map(Value::String).unwrap_or(Value::Null);
                    } else if kind == "fs.entry.duplicate.request" {
                        payload["duplicatedPath"] =
                            result.ok().map(Value::String).unwrap_or(Value::Null);
                    }
                }
                context
                    .send(json!({"type":kind.replace(".request",".response"),"payload":payload}))
                    .await?;
            }
            "file_download_token_request" => {
                let r = request.clone();
                let result:Result<Value>=async {
                    let file=tokio::task::spawn_blocking(move||fs::open(fs::field(&r,"cwd")?.trim(),fs::field(&r,"path")?,&home)).await??;
                    let name=file.scope.requested.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let token=self.downloads.get().ok_or_else(||anyhow!("File service is not initialized"))?.issue(file.scope.resolved,file.scope.relative.clone(),name.clone(),file.mime.into(),file.metadata.len()).await?;
                    Ok(json!({"cwd":request["cwd"].as_str().unwrap_or("").trim(),"path":file.scope.relative,"token":token,"fileName":name,"mimeType":file.mime,"size":file.metadata.len(),"error":null,"requestId":request["requestId"]}))
                }.await;
                let payload=result.unwrap_or_else(|error|json!({"cwd":trimmed_cwd(request),"path":request["path"],"token":null,"fileName":null,"mimeType":null,"size":null,"error":error.to_string(),"requestId":request["requestId"]}));
                context
                    .send(json!({"type":"file_download_token_response","payload":payload}))
                    .await?;
            }
            "file_explorer_request" => {
                let result = self.explore(context, request, home).await;
                if let Err(error) = result {
                    context.send(json!({"type":"file_explorer_response","payload":{"cwd":trimmed_cwd(request),"path":request.get("path").cloned().unwrap_or(json!(".")),"mode":request["mode"],"directory":null,"file":null,"error":error.to_string(),"requestId":request["requestId"]}})).await?;
                }
            }
            _ => unreachable!(),
        }
        Ok(true)
    }
    async fn explore(
        &self,
        context: &RequestContext,
        request: &Value,
        home: PathBuf,
    ) -> Result<()> {
        let cwd = fs::field(request, "cwd")?.trim().to_owned();
        let path = request["path"].as_str().unwrap_or(".").to_owned();
        let cancel = context.cancellation();
        let max = request["maxBytes"].as_u64();
        let (c, p) = (cwd.clone(), path.clone());
        if request["mode"] == "list" {
            let directory =
                tokio::task::spawn_blocking(move || fs::list(&c, &p, &home, &cancel)).await??;
            context.send(json!({"type":"file_explorer_response","payload":{"cwd":cwd,"path":directory["path"],"mode":"list","directory":directory,"file":null,"error":null,"requestId":request["requestId"]}})).await?;
        } else if request["acceptBinary"] == true {
            let opened =
                tokio::task::spawn_blocking(move || fs::stream_open(&c, &p, &home, max, &cancel))
                    .await??;
            let id = fs::field(request, "requestId")?;
            let revision = fs::revision(&opened.file, &opened.metadata)?;
            let size = opened.metadata.len();
            let metadata = serde_json::to_vec(
                &json!({"mime":opened.mime,"size":size,"encoding":if opened.kind=="text"{"utf-8"}else{"binary"},"modifiedAt":fs::modified(&opened.metadata)?,"revision":revision}),
            )?;
            context
                .send_binary(transfer::encode_frame(0x10, id, &metadata)?)
                .await?;
            let mut file = tokio::fs::File::from_std(opened.file);
            let mut remaining = size;
            let mut chunk = vec![0u8; fs::CHUNK];
            let cancel = context.cancellation();
            while remaining > 0 {
                let length = remaining.min(chunk.len() as u64) as usize;
                let count = tokio::select! {biased;_=cancel.cancelled()=>return Err(anyhow!("Physical source is closed")),result=file.read(&mut chunk[..length])=>result?};
                ensure!(count > 0, "File changed during transfer");
                remaining -= count as u64;
                context
                    .send_binary(transfer::encode_frame(0x11, id, &chunk[..count])?)
                    .await?;
            }
            let file = file.into_std().await;
            tokio::task::spawn_blocking(move || -> Result<()> {
                ensure!(
                    fs::revision(&file, &file.metadata()?)? == revision,
                    "File changed during transfer"
                );
                // Atomic replacement leaves the old handle readable. Reopen the
                // advertised path so its identity is checked, not the old handle twice.
                let current = fs::open_no_follow(&opened.scope.resolved)?;
                ensure!(
                    fs::revision(&current, &current.metadata()?)? == revision,
                    "File changed during transfer"
                );
                Ok(())
            })
            .await??;
            context
                .send_binary(transfer::encode_frame(0x12, id, &[])?)
                .await?;
        } else {
            let file = tokio::task::spawn_blocking(move || fs::read(&c, &p, &home, max, &cancel))
                .await??;
            context.send(json!({"type":"file_explorer_response","payload":{"cwd":cwd,"path":file["path"],"mode":"file","directory":null,"file":file,"error":null,"requestId":request["requestId"]}})).await?;
        }
        Ok(())
    }
    async fn subscribe(
        &self,
        context: &RequestContext,
        request: &Value,
        home: PathBuf,
    ) -> Result<()> {
        let legacy = format!("file:{}", request["subscriptionId"].as_str().unwrap_or(""));
        let owner = context.begin_subscription(
            "files",
            request["subscriptionId"].as_str(),
            Some(&legacy),
        )?;
        let cwd = fs::field(request, "cwd")?.to_owned();
        let path = fs::field(request, "path")?.to_owned();
        match self
            .observers
            .subscribe(cwd.clone(), path.clone(), home)
            .await
        {
            Ok(mut observation) => {
                let initial = observation.receiver.borrow_and_update().clone();
                let identity_cwd = initial["cwd"].clone();
                let identity_path = initial["path"].clone();
                if let Err(error)=context.send(json!({"type":"fs.file.subscribe.response","payload":{"subscriptionId":owner.response_id(),"initial":initial,"requestId":request["requestId"]}})).await {owner.release().await?;return Err(error);}
                owner.activate().await?;
                tokio::spawn(async move {
                    let cancel = owner.cancellation();
                    loop {
                        tokio::select! {biased;_=cancel.cancelled()=>break,result=observation.receiver.changed()=>{if result.is_err(){break;}}}
                        let mut version = observation.receiver.borrow_and_update().clone();
                        version["cwd"] = identity_cwd.clone();
                        version["path"] = identity_path.clone();
                        if owner.emit(json!({"type":"fs.file.update","payload":{"subscriptionId":owner.response_id(),"version":version}})).await.is_err(){break;}
                    }
                    let _ = owner.release().await;
                });
            }
            Err(error) => {
                owner.release().await?;
                context.send(json!({"type":"fs.file.subscribe.response","payload":{"subscriptionId":owner.response_id(),"initial":{"status":"error","cwd":cwd,"path":path,"error":error.to_string()},"requestId":request["requestId"]}})).await?;
            }
        }
        Ok(())
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        self.retention_cancel.cancel();
    }
}

/// The source echoes the trimmed workspace cwd on failures, except the
/// "cwd is required" case, which echoes the request value unchanged.
fn trimmed_cwd(request: &Value) -> Value {
    match request["cwd"].as_str().map(str::trim) {
        Some(cwd) if !cwd.is_empty() => Value::String(cwd.to_owned()),
        _ => request["cwd"].clone(),
    }
}
