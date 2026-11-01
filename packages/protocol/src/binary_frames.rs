//! Network-order binary multiplexing. Decoders borrow payloads and reject
//! truncated lengths, invalid UTF-8 identifiers and unrecognized opcodes.
use crate::{MAX_SAFE_INTEGER, MAX_WEBSOCKET_MESSAGE_BYTES, ProtocolError};
use serde::{Deserialize, Serialize};

pub const HOST_ROUTE_OPCODE: u8 = 0x30;
pub const MAX_HOST_ROUTE_ID_BYTES: usize = 256;
pub const MAX_HOST_ROUTE_HEADER_BYTES: usize = 3 + MAX_HOST_ROUTE_ID_BYTES;
pub const MAX_ROUTED_PAYLOAD_BYTES: usize =
    MAX_WEBSOCKET_MESSAGE_BYTES - MAX_HOST_ROUTE_HEADER_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalOpcode {
    Output = 1,
    Input = 2,
    Resize = 3,
    Snapshot = 4,
    Restore = 5,
}
impl TryFrom<u8> for TerminalOpcode {
    type Error = ProtocolError;
    fn try_from(op: u8) -> Result<Self, Self::Error> {
        match op {
            1 => Ok(Self::Output),
            2 => Ok(Self::Input),
            3 => Ok(Self::Resize),
            4 => Ok(Self::Snapshot),
            5 => Ok(Self::Restore),
            _ => Err(ProtocolError::Invalid("terminal opcode")),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalFrame<'a> {
    pub opcode: TerminalOpcode,
    pub slot: u8,
    pub payload: &'a [u8],
}
pub fn decode_terminal_frame(bytes: &[u8]) -> Result<TerminalFrame<'_>, ProtocolError> {
    if bytes.len() < 2 {
        return Err(ProtocolError::Invalid("terminal header"));
    }
    Ok(TerminalFrame {
        opcode: bytes[0].try_into()?,
        slot: bytes[1],
        payload: &bytes[2..],
    })
}
pub fn encode_terminal_frame(opcode: TerminalOpcode, slot: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + payload.len());
    bytes.extend_from_slice(&[opcode as u8, slot]);
    bytes.extend_from_slice(payload);
    bytes
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResizeIntent {
    Claim,
    Update,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalResize {
    pub rows: u32,
    pub cols: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<ResizeIntent>,
}
pub fn decode_terminal_resize(bytes: &[u8]) -> Result<TerminalResize, ProtocolError> {
    let value: TerminalResize = serde_json::from_slice(bytes).map_err(|_| ProtocolError::Json)?;
    if value.rows == 0 || value.cols == 0 {
        return Err(ProtocolError::Invalid("terminal size"));
    }
    Ok(value)
}
pub fn encode_terminal_resize(value: &TerminalResize) -> Result<Vec<u8>, ProtocolError> {
    if value.rows == 0 || value.cols == 0 {
        return Err(ProtocolError::Invalid("terminal size"));
    }
    serde_json::to_vec(value).map_err(|_| ProtocolError::Json)
}
pub fn decode_terminal_snapshot(bytes: &[u8]) -> Result<serde_json::Value, ProtocolError> {
    let value = serde_json::from_slice(bytes).map_err(|_| ProtocolError::Json)?;
    crate::wire::validate_terminal_snapshot(&value)?;
    Ok(value)
}
pub fn encode_terminal_snapshot(value: &serde_json::Value) -> Result<Vec<u8>, ProtocolError> {
    crate::wire::validate_terminal_snapshot(value)?;
    serde_json::to_vec(value).map_err(|_| ProtocolError::Json)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    #[serde(rename = "binary")]
    Binary,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBeginMetadata {
    pub mime: String,
    pub size: u64,
    pub encoding: FileEncoding,
    pub modified_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}
impl FileBeginMetadata {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.mime.is_empty() || self.size > MAX_SAFE_INTEGER {
            return Err(ProtocolError::Invalid("file metadata"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferFrame<'a> {
    Begin {
        request_id: &'a str,
        metadata: FileBeginMetadata,
    },
    Chunk {
        request_id: &'a str,
        payload: &'a [u8],
    },
    End {
        request_id: &'a str,
    },
}
impl<'a> FileTransferFrame<'a> {
    pub fn request_id(&self) -> &'a str {
        match self {
            Self::Begin { request_id, .. }
            | Self::Chunk { request_id, .. }
            | Self::End { request_id } => request_id,
        }
    }
}
pub fn decode_file_transfer_frame(bytes: &[u8]) -> Result<FileTransferFrame<'_>, ProtocolError> {
    if bytes.len() < 2 || !matches!(bytes[0], 0x10..=0x12) {
        return Err(ProtocolError::Invalid("file header"));
    }
    let n = bytes[1] as usize;
    if n == 0 || n > bytes.len() - 2 {
        return Err(ProtocolError::Invalid("requestId"));
    }
    let request_id =
        std::str::from_utf8(&bytes[2..2 + n]).map_err(|_| ProtocolError::Invalid("requestId"))?;
    let body = &bytes[2 + n..];
    match bytes[0] {
        0x10 => {
            if body.len() < 2 || u16::from_be_bytes([body[0], body[1]]) as usize != body.len() - 2 {
                return Err(ProtocolError::Invalid("metadata length"));
            }
            let metadata: FileBeginMetadata =
                serde_json::from_slice(&body[2..]).map_err(|_| ProtocolError::Json)?;
            metadata.validate()?;
            Ok(FileTransferFrame::Begin {
                request_id,
                metadata,
            })
        }
        0x11 => Ok(FileTransferFrame::Chunk {
            request_id,
            payload: body,
        }),
        _ if body.is_empty() => Ok(FileTransferFrame::End { request_id }),
        _ => Err(ProtocolError::Invalid("file end payload")),
    }
}
pub fn encode_file_transfer_frame(frame: &FileTransferFrame<'_>) -> Result<Vec<u8>, ProtocolError> {
    let id = frame.request_id().as_bytes();
    if id.is_empty() || id.len() > 255 {
        return Err(ProtocolError::Invalid("requestId"));
    }
    let (opcode, metadata, payload) = match frame {
        FileTransferFrame::Begin { metadata, .. } => {
            metadata.validate()?;
            let bytes = serde_json::to_vec(metadata).map_err(|_| ProtocolError::Json)?;
            if bytes.len() > u16::MAX as usize {
                return Err(ProtocolError::Invalid("metadata length"));
            }
            (0x10, Some(bytes), &[][..])
        }
        FileTransferFrame::Chunk { payload, .. } => (0x11, None, *payload),
        FileTransferFrame::End { .. } => (0x12, None, &[][..]),
    };
    let body_length = metadata
        .as_ref()
        .map_or(payload.len(), |bytes| 2 + bytes.len());
    let mut bytes = Vec::with_capacity(2 + id.len() + body_length);
    bytes.extend_from_slice(&[opcode, id.len() as u8]);
    bytes.extend_from_slice(id);
    if let Some(metadata) = metadata {
        bytes.extend_from_slice(&(metadata.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&metadata);
    } else {
        bytes.extend_from_slice(payload);
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRouteFrame<'a> {
    pub route_id: &'a str,
    pub payload: &'a [u8],
}
pub fn decode_host_route_frame(bytes: &[u8]) -> Result<HostRouteFrame<'_>, ProtocolError> {
    if bytes.len() < 3 || bytes[0] != HOST_ROUTE_OPCODE {
        return Err(ProtocolError::Invalid("host route header"));
    }
    let n = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    if n == 0 || n > MAX_HOST_ROUTE_ID_BYTES || n > bytes.len() - 3 {
        return Err(ProtocolError::Invalid("routeId"));
    }
    if bytes.len() - 3 - n > MAX_ROUTED_PAYLOAD_BYTES {
        return Err(ProtocolError::WireLimit);
    }
    let route_id =
        std::str::from_utf8(&bytes[3..3 + n]).map_err(|_| ProtocolError::Invalid("routeId"))?;
    Ok(HostRouteFrame {
        route_id,
        payload: &bytes[3 + n..],
    })
}
pub fn encode_host_route_frame(route_id: &str, payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if route_id.is_empty() || route_id.len() > MAX_HOST_ROUTE_ID_BYTES {
        return Err(ProtocolError::Invalid("routeId"));
    }
    if payload.len() > MAX_ROUTED_PAYLOAD_BYTES {
        return Err(ProtocolError::WireLimit);
    }
    let mut bytes = Vec::with_capacity(3 + route_id.len() + payload.len());
    bytes.push(HOST_ROUTE_OPCODE);
    bytes.extend_from_slice(&(route_id.len() as u16).to_be_bytes());
    bytes.extend_from_slice(route_id.as_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryFrame<'a> {
    Terminal(TerminalFrame<'a>),
    FileTransfer(FileTransferFrame<'a>),
    HostRoute(HostRouteFrame<'a>),
}
pub fn decode_binary_frame(bytes: &[u8]) -> Result<BinaryFrame<'_>, ProtocolError> {
    if bytes.len() > MAX_WEBSOCKET_MESSAGE_BYTES {
        return Err(ProtocolError::WireLimit);
    }
    match bytes.first() {
        Some(1..=5) => decode_terminal_frame(bytes).map(BinaryFrame::Terminal),
        Some(0x10..=0x12) => decode_file_transfer_frame(bytes).map(BinaryFrame::FileTransfer),
        Some(0x30) => decode_host_route_frame(bytes).map(BinaryFrame::HostRoute),
        _ => Err(ProtocolError::Invalid("binary opcode")),
    }
}
