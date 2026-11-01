//! CLI-local process identity checks; wire transport and requests live in paseo-client.
use crate::daemon_control::{self, DaemonInstance};
use anyhow::Result;
use paseo_client::{ClientError, DaemonClient, DaemonTarget};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct ConnectOptions {
    pub target: DaemonTarget,
    pub timeout: Duration,
    pub instance: Option<DaemonInstance>,
    pub client_type: String,
}
impl ConnectOptions {
    pub fn new(target: DaemonTarget) -> Self {
        Self { target, timeout: Duration::from_secs(15), instance: None, client_type: "cli".into() }
    }
}
pub async fn connect(options: ConnectOptions, cancel: CancellationToken) -> Result<DaemonClient> {
    let mut shared = paseo_client::ConnectOptions::new(options.target.clone());
    shared.timeout = options.timeout;
    shared.client_type = options.client_type;
    if let DaemonTarget::Instance { home } = &options.target {
        let instance = daemon_control::wait_for_daemon_ready(home, options.timeout, options.instance.as_ref(), &cancel).await?;
        let host = instance.listen.ok_or_else(|| ClientError::new("DAEMON_NOT_READY", "Daemon has not published a listening address"))?;
        shared.target = DaemonTarget::Endpoint { host };
        shared.home = Some(home.clone());
    }
    DaemonClient::connect(shared, cancel).await
}
