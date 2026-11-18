//! Same-user desktop update IPC. Only the desktop owner may dispatch these methods.

#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

use crate::ClientError;
use anyhow::Result;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{future::Future, io, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    task::JoinSet,
    time::timeout,
};

const REQUEST_BYTES: usize = 4 * 1024;
const RESPONSE_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_ACTIVE: usize = 4;
const RECEIVED: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Method {
    Check,
    Submit,
    Status,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<T> {
    method: Method,
    args: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Refusal {
    Unavailable,
    OperationNotFound,
    OutcomeUnknown,
    Failed,
}

impl From<Refusal> for ClientError {
    fn from(value: Refusal) -> Self {
        let (code, message) = match value {
            Refusal::Unavailable => (
                "DESKTOP_APP_UNAVAILABLE",
                "The desktop app on this host is not available for remote updates.",
            ),
            Refusal::OperationNotFound => (
                "DESKTOP_APP_OPERATION_NOT_FOUND",
                "This desktop app update operation was not found on the host.",
            ),
            Refusal::OutcomeUnknown => (
                "DESKTOP_APP_OUTCOME_UNKNOWN",
                "The desktop app update outcome is unknown. Check the existing operation before submitting another update.",
            ),
            Refusal::Failed => (
                "DESKTOP_APP_UPDATE_FAILED",
                "The desktop app could not complete the update request on this host.",
            ),
        };
        Self::new(code, message)
    }
}

type Reply = std::result::Result<Value, Refusal>;

/// One request to the desktop owner of the exact daemon home. Never retries an update.
pub async fn request(home: &Path, method: Method, args: &Value) -> Result<Value> {
    let frame = encode(&Request { method, args }, REQUEST_BYTES)
        .map_err(|_| ClientError::from(Refusal::Failed))?;
    let mut sent = false;
    let result = timeout(REQUEST_TIMEOUT, async {
        let mut stream = timeout(IO_TIMEOUT, sys::connect(home)).await??;
        timeout(IO_TIMEOUT, async {
            let written = stream.write(&frame).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            sent = true;
            stream.write_all(&frame[written..]).await
        })
        .await??;
        let reply: Reply = read_message(&mut stream, RESPONSE_BYTES).await?;
        Ok::<_, anyhow::Error>((stream, reply))
    })
    .await;
    let Ok(Ok((mut stream, reply))) = result else {
        let refusal = if method == Method::Submit && sent {
            Refusal::OutcomeUnknown
        } else {
            Refusal::Unavailable
        };
        return Err(ClientError::from(refusal).into());
    };
    // Windows may discard unread output when the server closes its pipe handle.
    // A failed acknowledgement does not change the completed reply.
    let _ = timeout(IO_TIMEOUT, stream.write_u8(RECEIVED)).await;
    Ok(reply.map_err(ClientError::from)?)
}

/// Held by the running desktop process, not the daemon. Dropping it releases the endpoint.
pub struct Owner(sys::Listener);

impl Owner {
    pub async fn bind(home: &Path) -> Result<Self> {
        Ok(Self(sys::Listener::bind(home).await?))
    }

    pub async fn serve<F, Fut>(mut self, dispatch: F) -> Result<()>
    where
        F: Fn(Method, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<Value, Refusal>> + Send + 'static,
    {
        let dispatch = Arc::new(dispatch);
        let mut active = JoinSet::new();
        loop {
            if active.len() == MAX_ACTIVE {
                active.join_next().await;
                continue;
            }
            tokio::select! {
                _ = active.join_next(), if !active.is_empty() => {}
                stream = self.0.accept() => {
                    if let Some(stream) = stream? {
                        let dispatch = dispatch.clone();
                        active.spawn(serve_one(stream, dispatch));
                    }
                }
            }
        }
    }
}

async fn serve_one<S, F, Fut>(mut stream: S, dispatch: Arc<F>)
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(Method, Value) -> Fut,
    Fut: Future<Output = Reply>,
{
    let Ok(Ok(request)) = timeout(
        IO_TIMEOUT,
        read_message::<Request<Value>>(&mut stream, REQUEST_BYTES),
    )
    .await
    else {
        return;
    };
    // A submitted operation keeps its journal authority if the requester disconnects.
    let reply = dispatch(request.method, request.args).await;
    let Ok(frame) = encode(&reply, RESPONSE_BYTES).or_else(|_| {
        let refusal = if request.method == Method::Submit {
            Refusal::OutcomeUnknown
        } else {
            Refusal::Failed
        };
        encode(&Reply::Err(refusal), RESPONSE_BYTES)
    }) else {
        return;
    };
    let _ = timeout(IO_TIMEOUT, async {
        stream.write_all(&frame).await?;
        stream.read_u8().await
    })
    .await;
}

async fn read_message<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> io::Result<T> {
    let len = stream.read_u32().await? as usize;
    if len == 0 || len > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn encode(value: &impl Serialize, limit: usize) -> serde_json::Result<Vec<u8>> {
    struct Frame {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl io::Write for Frame {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    const HEADER_BYTES: usize = std::mem::size_of::<u32>();
    let mut frame = Frame {
        bytes: vec![0; HEADER_BYTES],
        limit: HEADER_BYTES + limit,
    };
    serde_json::to_writer(&mut frame, value)?;
    let length = (frame.bytes.len() - HEADER_BYTES) as u32;
    frame.bytes[..HEADER_BYTES].copy_from_slice(&length.to_be_bytes());
    Ok(frame.bytes)
}

#[cfg(test)]
mod tests;
