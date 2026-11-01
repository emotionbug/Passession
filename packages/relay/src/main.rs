use clap::Parser;
use paseo_relay::{Config, Relay, router};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(
    version,
    about = "Paseo opaque WebSocket relay (TLS is terminated at ingress)"
)]
struct Args {
    #[arg(long, env = "PASEO_RELAY_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(long, env = "PASEO_RELAY_MAX_SESSIONS", default_value_t = 4096)]
    max_sessions: usize,
    #[arg(long, env = "PASEO_RELAY_MAX_SOCKETS", default_value_t = 8192)]
    max_sockets: usize,
    #[arg(long, env = "PASEO_RELAY_BUFFER_BYTES", default_value_t = 512 * 1024 * 1024)]
    buffer_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "paseo_relay=info".into()),
        )
        .init();
    let args = Args::parse();
    let relay = Relay::new(Config {
        max_sessions: args.max_sessions,
        max_sockets: args.max_sockets,
        total_buffer_bytes: args.buffer_bytes,
        ..Config::default()
    })?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "relay listening");
    let shutdown = relay.clone();
    axum::serve(listener, router(relay))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            shutdown.shutdown();
        })
        .await?;
    Ok(())
}
