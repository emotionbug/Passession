use crate::ClientError;
use anyhow::Result;
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

// Discovery only: the SDK never starts, stops or replaces a daemon process.
pub(crate) async fn listen(
    home: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let bytes = tokio::select! {
            _ = cancel.cancelled() => return Err(ClientError::new("DAEMON_REQUEST_CANCELLED", "Connection cancelled").into()),
            result = tokio::fs::read(home.join("paseo.pid")) => result.map_err(|error| ClientError::new("DAEMON_NOT_RUNNING", format!("Cannot discover daemon in {}: {error}", home.display())))?,
        };
        let record: Value = serde_json::from_slice(&bytes)?;
        if let Some(listen) = record["listen"].as_str().filter(|s| !s.is_empty()) {
            return Ok(listen.to_owned());
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err(ClientError::new("DAEMON_REQUEST_CANCELLED", "Connection cancelled").into()),
            _ = tokio::time::sleep_until(deadline) => return Err(ClientError::new("DAEMON_NOT_READY", "Daemon has not published its listening address").into()),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}
