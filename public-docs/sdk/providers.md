# Providers with the SDK

[中文](providers.zh-CN.md) · [Setup](quickstart.md)

## Configure a session

Use separate fields in creation: `"config": {"provider": "codex", "model": model_id, "cwd": directory}`. Model IDs may themselves contain slashes; do not split them or combine provider/model into a convenience string. Optional configuration includes `modeId`, `thinkingOptionId`, `featureValues`, and `providerOptions`. Agent launch depends on the provider's installation, credentials and configuration on the daemon host.

## Discover installed providers and models

```rust
use anyhow::Result;
use paseo_client::DaemonClient;
use serde_json::json;

async fn discover(client: &DaemonClient, cwd: &str) -> Result<()> {
    println!("{}", client.list_available_providers(json!({})).await?);
    println!("{}", client.get_providers_snapshot(json!({"cwd": cwd})).await?);
    let models = client.list_provider_models(json!({"provider": "codex", "cwd": cwd})).await?;
    println!("models: {}", models);
    let modes = client.list_provider_modes(json!({"provider": "codex", "cwd": cwd})).await?;
    println!("modes: {}", modes);
    let features = client.list_provider_features(json!({
        "draftConfig": {"provider": "codex", "cwd": cwd}
    })).await?;
    println!("features: {}", features);
    Ok(())
}
```

Queries use the daemon directory context, which can affect available models and authentication. `get_providers_snapshot` returns entries plus generation/hash metadata; `refresh_providers_snapshot(json!({"cwd": cwd, "providers": ["codex"]}))` explicitly refreshes a provider. A discovery response is not a promise that every provider is ready: inspect entry availability/errors. Subscribe to `providers_snapshot_update` as described in [Events](events.md) for later changes. There is no `waitForReady` facade in this API.

## Select a discovered model

Choose a model ID from the selected provider's returned `models`, then pass it unchanged to `config.model`. Do not hard-code a marketing name as a model ID or silently fall back to another provider. The snapshot can use compact/reference forms. Decode compact data explicitly:

```rust
use anyhow::{Context, Result};
use paseo_protocol::providers::{CompactProviderSnapshot, expand_provider_snapshot};
use serde_json::Value;

fn entries(payload: &Value) -> Result<Vec<Value>> {
    if let Some(compact) = payload.get("compactSnapshot").filter(|v| !v.is_null()) {
        let compact: CompactProviderSnapshot = serde_json::from_value(compact.clone())?;
        return Ok(expand_provider_snapshot(compact)?);
    }
    Ok(payload["entries"].as_array().context("missing provider entries")?.clone())
}
```

If you send `ifNoneMatch` and receive `notModified`, keep your matching cached snapshot rather than replacing it with empty entries. Cache by daemon, directory and snapshot hash; never interpret a hash-only notification as a full catalog. The static `paseo_protocol::providers::definitions()` catalog is metadata, not a probe of installed/authenticated providers.

## Discover modes, thinking levels, and features

`list_provider_modes` returns available modes. Models can carry `thinkingOptions` and `defaultThinkingOptionId`. `list_provider_features` takes `draftConfig`, so feature choices can reflect provider, model and mode. Use advertised IDs rather than labels. For an existing agent, `set_agent_mode`, `set_agent_model`, `set_agent_thinking_option`, `set_agent_feature` and `apply_agent_config` accept protocol-shaped fields; see the [request schema](../../packages/protocol/schema/session-inbound.json). Do not assume every provider supports every setting.

## Diagnose an unavailable provider

`client.get_provider_diagnostic(json!({"provider": "codex"})).await?` requests diagnostic details. Use its returned state/errors to fix the external executable or authentication on the daemon host; reconnecting the SDK does not install providers. `get_daemon_config` and `patch_daemon_config` expose config-backed provider settings, profiles, environment, binaries and extra models. Review the current configuration before patching and never log credentials. Provider options and host isolation are [separate concerns](provider-options.md).
