use crate::{ClientError, DaemonClient, Pending, PendingGuard, RequestOptions};
use anyhow::{Result, bail, ensure};
use base64::Engine;
use bytes::Bytes;
use paseo_protocol::binary_frames::{
    FileBeginMetadata, FileEncoding, FileTransferFrame, decode_file_transfer_frame,
    encode_file_transfer_frame,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct DownloadedFile {
    pub metadata: FileBeginMetadata,
    pub bytes: Bytes,
}

impl DaemonClient {
    /// Read through the binary protocol, accepting legacy JSON responses as well.
    /// The byte ceiling applies before allocating and to every arriving chunk.
    pub async fn read_file(
        &self,
        cwd: &str,
        path: &str,
        max_bytes: usize,
        options: RequestOptions,
    ) -> Result<DownloadedFile> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut binary = self.subscribe_binary();
        let request = self.request_cancellable(json!({"type":"file_explorer_request","cwd":cwd,"path":path,"mode":"file","acceptBinary":true,"maxBytes":max_bytes,"requestId":id}), "file_explorer_response", options.timeout, options.cancel.clone());
        tokio::pin!(request);
        let mut metadata: Option<FileBeginMetadata> = None;
        let mut data = Vec::new();
        loop {
            tokio::select! { biased;
                result = &mut request => {
                    let payload = result?;
                    let file = payload.get("file").unwrap_or(&payload);
                    let content = file["content"].as_str().ok_or_else(|| anyhow::anyhow!("File response omitted content"))?;
                    ensure!(content.len() <= max_bytes.saturating_mul(4).saturating_div(3).saturating_add(4), "File exceeds byte ceiling");
                    let bytes = if file["encoding"] == "base64" { base64::engine::general_purpose::STANDARD.decode(content)? } else { content.as_bytes().to_vec() };
                    ensure!(bytes.len() <= max_bytes, "File exceeds byte ceiling");
                    return Ok(DownloadedFile { metadata: FileBeginMetadata { mime: file["mimeType"].as_str().unwrap_or("application/octet-stream").into(), size: bytes.len() as u64, encoding: if file["encoding"] == "base64" { FileEncoding::Binary } else { FileEncoding::Utf8 }, modified_at: file["modifiedAt"].as_str().unwrap_or("").into(), revision: file["revision"].as_str().map(str::to_owned), file_name: None }, bytes: bytes.into() });
                },
                frame = binary.recv() => {
                    let frame = frame?;
                    // Other binary channels share this broadcast.
                    if frame.first().is_none_or(|byte| !matches!(*byte, 0x10..=0x12)) { continue; }
                    match decode_file_transfer_frame(&frame)? {
                        FileTransferFrame::Begin {request_id, metadata: next} if request_id == id => {
                            ensure!(metadata.is_none(), "Duplicate file begin");
                            ensure!(next.size <= max_bytes as u64, "File exceeds byte ceiling");
                            metadata = Some(next);
                        },
                        FileTransferFrame::Chunk {request_id, payload} if request_id == id => {
                            let meta = metadata.as_ref().ok_or_else(|| anyhow::anyhow!("File chunk preceded begin"))?;
                            ensure!(payload.len() <= max_bytes.saturating_sub(data.len()) && data.len() as u64 + payload.len() as u64 <= meta.size, "File chunk exceeds declared size");
                            data.extend_from_slice(payload);
                        },
                        FileTransferFrame::End {request_id} if request_id == id => {
                            let metadata = metadata.ok_or_else(|| anyhow::anyhow!("File end preceded begin"))?;
                            ensure!(data.len() as u64 == metadata.size, "Incomplete file transfer");
                            return Ok(DownloadedFile { metadata, bytes: data.into() });
                        },
                        _ => {}
                    }
                }
            }
        }
    }

    /// Upload bytes in bounded frames. A cancelled upload is not retried automatically.
    pub async fn upload_file(
        &self,
        file_name: &str,
        mime: &str,
        modified_at: &str,
        bytes: &[u8],
        options: RequestOptions,
    ) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (reply, mut response) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(
            id.clone(),
            Pending {
                response: "file.upload.response".into(),
                reply,
            },
        );
        let _guard = PendingGuard {
            state: self.shared.clone(),
            id: id.clone(),
        };
        let transfer = async {
            self.send(json!({"type":"file.upload.request","requestId":id,"fileName":file_name,"mimeType":mime,"size":bytes.len(),"modifiedAt":modified_at})).await?;
            let metadata = FileBeginMetadata {
                mime: mime.into(),
                size: bytes.len() as u64,
                encoding: FileEncoding::Binary,
                modified_at: modified_at.into(),
                revision: None,
                file_name: Some(file_name.into()),
            };
            self.send_binary(
                encode_file_transfer_frame(&FileTransferFrame::Begin {
                    request_id: &id,
                    metadata,
                })?
                .into(),
            )
            .await?;
            for payload in bytes.chunks(128 * 1024) {
                self.send_binary(
                    encode_file_transfer_frame(&FileTransferFrame::Chunk {
                        request_id: &id,
                        payload,
                    })?
                    .into(),
                )
                .await?;
            }
            self.send_binary(
                encode_file_transfer_frame(&FileTransferFrame::End { request_id: &id })?.into(),
            )
            .await?;
            std::future::pending::<Result<()>>().await
        };
        tokio::select! {biased;
            _=options.cancel.cancelled()=>Err(ClientError::new("DAEMON_REQUEST_CANCELLED","Upload cancelled; submitted outcome may be unknown").into()),
            _=self.shared.cancel.cancelled()=>Err(ClientError::new("DAEMON_CONNECTION_LOST","Upload connection closed").into()),
            _=async {if options.timeout.is_zero(){std::future::pending::<()>().await}else{tokio::time::sleep(options.timeout).await}}=>Err(ClientError::new("DAEMON_REQUEST_TIMEOUT","Upload timed out; submitted outcome may be unknown").into()),
            result=&mut response=>result.map_err(|_|anyhow::anyhow!("Upload response closed"))?,
            result=transfer=>{result?;bail!("Upload stopped without response")}
        }
    }
}
