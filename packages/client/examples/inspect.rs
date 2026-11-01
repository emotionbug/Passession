//! Read-only example: cargo run -p paseo-client --example inspect -- ws://127.0.0.1:6767/ws
//! With no endpoint, use the existing daemon discovered under PASEO_HOME.
//! PASEO_PASSWORD or the matching local credential supplies authentication.
use anyhow::Result;
use paseo_client::{ConnectOptions, CursorPages, DaemonClient, DaemonTarget, RequestOptions};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = std::env::args().nth(1);
    let target = DaemonTarget::select(None, endpoint.as_deref(), false)?;
    let cancel = CancellationToken::new();
    let interrupt = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupt.cancel();
        }
    });
    let client = DaemonClient::connect(ConnectOptions::new(target), cancel.child_token()).await?;
    let result = async {
        println!(
            "Daemon: {}",
            serde_json::to_string_pretty(&client.server_info())?
        );
        println!(
            "Round trip: {:?}",
            client.ping(Duration::from_secs(5)).await?
        );
        let mut pages = CursorPages::agents(json!({}));
        let options = RequestOptions {
            timeout: Duration::from_secs(30),
            cancel: cancel.child_token(),
        };
        while let Some(page) = pages.next(&client, options.clone()).await? {
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    client.close().await;
    signal.abort();
    result
}
