//! Transport-independent Paseo contracts. JSON Schema assets are the native wire
//! authority; extension fields are preserved rather than discarded by DTO casts.
//! Crypto is socket-independent and never performs application authentication.
pub mod binary_frames;
pub mod crypto;
pub mod helpers;
pub mod providers;
pub mod routing;
pub mod timeline;
pub mod wire;

/// Direct daemon WebSocket message limit; relay encryption has its own limit.
pub const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid JSON message")]
    Json,
    #[error("message exceeds wire limit")]
    WireLimit,
    #[error("invalid protocol field: {0}")]
    Invalid(&'static str),
    #[error("unknown message type")]
    UnknownMessage,
    #[error("message does not match the wire schema at {0}")]
    Schema(String),
    #[error("invalid connection endpoint")]
    Endpoint,
}
