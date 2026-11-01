//! Explicit, read-only relay probes. No production endpoint is contacted by builds/tests.
use anyhow::{Context, Result, ensure};
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

fn options(args: &[String]) -> Result<BTreeMap<String, String>> {
    ensure!(
        args.len() % 2 == 0,
        "Probe options require --name value pairs"
    );
    let mut options = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        ensure!(
            matches!(
                pair[0].as_str(),
                "--server-id"
                    | "--daemon-public-key-b64"
                    | "--relay-endpoint"
                    | "--timeout-ms"
                    | "--stability-ms"
                    | "--direct-endpoint"
                    | "--count"
                    | "--warmup"
            ),
            "Unknown probe option {}",
            pair[0]
        );
        ensure!(!pair[1].is_empty(), "Empty probe option {}", pair[0]);
        ensure!(
            options
                .insert(pair[0][2..].to_owned(), pair[1].clone())
                .is_none(),
            "Duplicate probe option {}",
            pair[0]
        );
    }
    Ok(options)
}
fn number(options: &BTreeMap<String, String>, name: &str, env: &str, default: u64) -> Result<u64> {
    let value = options
        .get(name)
        .cloned()
        .or_else(|| std::env::var(env).ok());
    value
        .map(|v| v.parse().with_context(|| format!("Invalid {name}")))
        .unwrap_or(Ok(default))
}
async fn connect(
    host: String,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<DaemonClient> {
    let mut options = ConnectOptions::new(DaemonTarget::Endpoint { host });
    options.timeout = timeout;
    options.password = std::env::var("PASEO_PASSWORD").ok();
    options.client_id = Some(format!("paseo-native-probe-{}", std::process::id()));
    DaemonClient::connect(options, cancel.child_token()).await
}
async fn ping(
    client: &DaemonClient,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(f64, i64)> {
    let sent = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let start = Instant::now();
    let result = tokio::select! {
        result = client.request(json!({"type":"ping","clientSentAt":sent}), "pong", timeout) => result?,
        _ = cancel.cancelled() => anyhow::bail!("Probe cancelled"),
    };
    let payload = result.get("payload").unwrap_or(&result);
    let received = payload["serverReceivedAt"]
        .as_i64()
        .context("Pong missing serverReceivedAt")?;
    let sent = payload["serverSentAt"]
        .as_i64()
        .context("Pong missing serverSentAt")?;
    Ok((start.elapsed().as_secs_f64() * 1000.0, sent - received))
}
fn statistics(samples: &[(f64, i64)]) -> Value {
    let mut sorted: Vec<f64> = samples.iter().map(|v| v.0).collect();
    sorted.sort_by(f64::total_cmp);
    let count = samples.len();
    json!({"samples":count,"minMs":sorted[0],"maxMs":sorted[count-1],"averageMs":sorted.iter().sum::<f64>()/count as f64,"p50Ms":sorted[count/2],"p95Ms":sorted[(count*95/100).min(count-1)],"allMs":samples.iter().map(|v|v.0).collect::<Vec<_>>(),"serverProcessingMs":samples.iter().map(|v|v.1).collect::<Vec<_>>(),"serverProcessingAverageMs":samples.iter().map(|v|v.1 as f64).sum::<f64>()/count as f64})
}
async fn measure(
    client: &DaemonClient,
    count: u64,
    warmup: u64,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    for _ in 0..warmup {
        ping(client, timeout, cancel).await?;
    }
    let mut samples = Vec::new();
    for _ in 0..count {
        samples.push(ping(client, timeout, cancel).await?);
        tokio::select! { _ = cancel.cancelled() => anyhow::bail!("Probe cancelled"), _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
    }
    Ok(statistics(&samples))
}
pub(super) async fn run(
    production: bool,
    args: &[String],
    cancel: &CancellationToken,
) -> Result<()> {
    let args = options(args)?;
    let setting =
        |name: &str, env: &str| args.get(name).cloned().or_else(|| std::env::var(env).ok());
    let server = setting("server-id", "PASEO_SERVER_ID")
        .context("Provide --server-id or PASEO_SERVER_ID")?;
    let key = setting("daemon-public-key-b64", "PASEO_DAEMON_PUBLIC_KEY_B64")
        .context("Provide --daemon-public-key-b64 or PASEO_DAEMON_PUBLIC_KEY_B64")?;
    let endpoint = setting("relay-endpoint", "PASEO_RELAY_ENDPOINT")
        .unwrap_or_else(|| "relay.paseo.sh:443".into());
    let timeout = Duration::from_millis(number(
        &args,
        "timeout-ms",
        "PASEO_PROVE_TIMEOUT_MS",
        if production { 60_000 } else { 15_000 },
    )?);
    ensure!(!timeout.is_zero(), "timeout-ms must be positive");
    let mut relay_url = reqwest::Url::parse(&format!("relay://{endpoint}"))?;
    ensure!(
        relay_url.username().is_empty()
            && relay_url.password().is_none()
            && relay_url.query().is_none()
            && relay_url.fragment().is_none(),
        "Relay endpoint must be a host:port, not credentials or a URL"
    );
    relay_url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid relay endpoint"))?
        .clear()
        .push(&server);
    relay_url
        .query_pairs_mut()
        .append_pair("key", &key)
        .append_pair("ssl", "true")
        .append_pair("security", "protected");
    let relay = connect(relay_url.to_string(), timeout, cancel).await?;
    let result = async {
        if production {
            let stability = Duration::from_millis(number(&args, "stability-ms", "PASEO_PROVE_STABILITY_MS", 30_000)?);
            let started = Instant::now(); let mut samples = 0;
            loop {
                ping(&relay, timeout, cancel).await?; samples += 1;
                if started.elapsed() >= stability { break; }
                tokio::select! { _ = cancel.cancelled() => anyhow::bail!("Probe cancelled"), _ = tokio::time::sleep(Duration::from_secs(1).min(stability.saturating_sub(started.elapsed()))) => {} }
            }
            println!("{}", serde_json::to_string_pretty(&json!({"ok":true,"serverId":server,"relayEndpoint":endpoint,"stabilityMs":stability.as_millis(),"successfulPings":samples,"transport":"protected-relay"}))?);
        } else {
            let count = number(&args, "count", "PASEO_PROBE_COUNT", 20)?;
            let warmup = number(&args, "warmup", "PASEO_PROBE_WARMUP", 3)?;
            ensure!(count > 0, "count must be positive");
            let direct = connect(setting("direct-endpoint", "PASEO_DIRECT_ENDPOINT").unwrap_or_else(|| "localhost:6767".into()), timeout, cancel).await?;
            let direct_result = measure(&direct, count, warmup, timeout, cancel).await;
            direct.close().await;
            let direct_result = direct_result?;
            let relay_result = measure(&relay, count, warmup, timeout, cancel).await?;
            let mut raw = Vec::new();
            for index in 0..5 {
                let mut url = reqwest::Url::parse(&format!("wss://{endpoint}/ws"))?;
                url.query_pairs_mut().append_pair("serverId", &format!("latency_probe_{}_{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())).append_pair("role", "client").append_pair("clientId", &format!("probe_{index}"));
                let started = Instant::now();
                let (mut socket, _) = tokio::select! {
                    result = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url.as_str())) => result.context("Raw WebSocket connect timed out")??,
                    _ = cancel.cancelled() => anyhow::bail!("Probe cancelled"),
                };
                raw.push(started.elapsed().as_secs_f64() * 1000.0);
                tokio::time::timeout(timeout, socket.close(None)).await.context("Raw WebSocket close timed out")??;
            }
            println!("{}", serde_json::to_string_pretty(&json!({"direct":direct_result,"relay":relay_result,"rawWebSocketConnectMs":raw,"warmup":warmup}))?);
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    relay.close().await;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_latency_and_server_processing_independently() {
        let report = statistics(&[(9.0, 1), (1.0, 2), (5.0, 3)]);
        assert_eq!(report["minMs"], 1.0);
        assert_eq!(report["maxMs"], 9.0);
        assert_eq!(report["p50Ms"], 5.0);
        assert_eq!(report["averageMs"], 5.0);
        assert_eq!(report["serverProcessingAverageMs"], 2.0);
    }
}
