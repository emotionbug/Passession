# 在 SDK 中使用 Provider

[English](providers.md) · [环境配置](quickstart.zh-CN.md)

## 配置会话

创建时使用独立字段：`"config": {"provider": "codex", "model": model_id, "cwd": directory}`。模型 ID 本身可能含斜杠，不要拆分，也不要把 Provider/模型拼成便利字符串。可选配置包括 `modeId`、`thinkingOptionId`、`featureValues`、`providerOptions`。启动 Agent 依赖 daemon 主机上的 Provider 安装、凭据和配置。

## 发现已安装 Provider 和模型

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

查询使用 daemon 目录上下文，可能影响模型和认证。`get_providers_snapshot` 返回条目及生成时间/hash 元数据；`refresh_providers_snapshot(json!({"cwd": cwd, "providers": ["codex"]}))` 显式刷新 Provider。发现响应并不保证每个 Provider 都已就绪：检查条目可用性和错误。按[事件指南](events.zh-CN.md)订阅 `providers_snapshot_update` 以获取后续变化。此 API 没有 `waitForReady` 门面。

## 选择发现的模型

从所选 Provider 返回的 `models` 中选择模型 ID，原样传入 `config.model`。不要把营销名称硬编码为模型 ID，也不要静默改用其他 Provider。快照可能采用压缩/引用形式，需显式解码：

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

若发送 `ifNoneMatch` 并收到 `notModified`，应保留匹配的缓存快照，不要替换为空条目。缓存键应包含 daemon、目录和快照 hash；仅含 hash 的通知不是完整目录。静态 `paseo_protocol::providers::definitions()` 是元数据，不会探测已安装或已认证的 Provider。

## 发现模式、思考级别和功能

`list_provider_modes` 返回模式。模型可携带 `thinkingOptions`、`defaultThinkingOptionId`。`list_provider_features` 接受 `draftConfig`，使选项反映 Provider、模型和模式。使用公布的 ID，而不是标签。已有 Agent 可使用 `set_agent_mode`、`set_agent_model`、`set_agent_thinking_option`、`set_agent_feature`、`apply_agent_config`，参数采用[请求 schema](../../packages/protocol/schema/session-inbound.json)的形状。不要假定所有 Provider 都支持所有设置。

## 诊断不可用的 Provider

`client.get_provider_diagnostic(json!({"provider": "codex"})).await?` 获取诊断详情。根据返回的状态/错误，在 daemon 主机修复外部可执行文件或认证；重连 SDK 不会安装 Provider。`get_daemon_config`、`patch_daemon_config` 暴露基于配置的 Provider 设置、profiles、环境变量、自定义二进制和额外模型。修改前检查当前配置，不要记录凭据。Provider 选项与主机隔离是[不同问题](provider-options.zh-CN.md)。
