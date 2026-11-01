//! Shared native socket, encrypted relay, IPC and SSH transport.
pub mod connector;
pub mod target;
use anyhow::{Result, ensure};
use bytes::Bytes;
use tokio::sync::oneshot;
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub code: String,
    pub message: String,
}
impl HostError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
#[derive(Debug)]
pub enum HostFrame {
    Text(String),
    Binary(Bytes),
    Close { code: u16, reason: String },
    Flush(oneshot::Sender<Result<()>>),
}
impl HostFrame {
    pub fn len(&self) -> usize {
        match self {
            Self::Text(v) => v.len(),
            Self::Binary(v) => v.len(),
            Self::Close { reason, .. } => reason.len() + 2,
            Self::Flush(_) => 0,
        }
    }
}
pub fn encode_frame(id: &str, payload: &[u8]) -> Result<Bytes> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && payload.len() <= 100 * 1024 * 1024 - 259,
        "Invalid host route frame"
    );
    let mut bytes = bytes::BytesMut::with_capacity(3 + id.len() + payload.len());
    bytes.extend_from_slice(&[0x30]);
    bytes.extend_from_slice(&(id.len() as u16).to_be_bytes());
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes.freeze())
}
pub fn decode_frame(bytes: &[u8]) -> Result<(&str, usize)> {
    ensure!(
        bytes.len() >= 3 && bytes[0] == 0x30,
        "Invalid host route frame"
    );
    let length = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    ensure!(
        (1..=256).contains(&length)
            && bytes.len() >= 3 + length
            && bytes.len() - 3 - length <= 100 * 1024 * 1024 - 259,
        "Invalid host route frame"
    );
    Ok((std::str::from_utf8(&bytes[3..3 + length])?, 3 + length))
}
