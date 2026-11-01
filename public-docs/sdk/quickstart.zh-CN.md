# SDK 快速开始

[English](quickstart.md) · [SDK 概览](index.zh-CN.md)

使用 Rust 1.93 或更新版本。在 Paseo 仓库旁建立应用，使用以下路径依赖；请根据实际位置调整路径。这些说明不假定 crate 或新版本已经发布。

```toml
[package]
name = "paseo-integration"
version = "0.1.0"
edition = "2024"
rust-version = "1.93"

[dependencies]
paseo-client = { path = "../paseo/packages/client" }
paseo-protocol = { path = "../paseo/packages/protocol" }
anyhow = "1"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "time"] }
tokio-util = "0.7"
```

## 连接

按照[仓库说明](../../README.zh-CN.md)启动或选择自己的开发 daemon。SDK 连接不会启动 daemon。`DaemonTarget::select(None, None, false)` 读取 `PASEO_HOST` 或 `PASEO_HOME`，否则使用默认本地主目录。同时设置这两个环境变量会产生歧义，请显式选择一个。自动发现读取本地 daemon 公布的端点，并仅使用与该端点匹配的本地凭据。远程连接设置 `PASEO_HOST`，按需提供 daemon 密码。

## 创建 Agent

将下列代码保存为 `src/main.rs`。先在 daemon 所在主机安装并认证外部 Provider，按 [Provider](providers.zh-CN.md) 发现 Codex 模型，再将模型 ID 写入 `PASEO_MODEL`。工作目录属于 daemon 所在主机；远程使用时替换 `current_dir()`。

```rust
use anyhow::{Context, Result};
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget, RequestOptions};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let target = DaemonTarget::select(None, None, false)?;
    let mut options = ConnectOptions::new(target);
    options.password = std::env::var("PASEO_PASSWORD").ok();
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;

    let result: Result<()> = async {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let model = std::env::var("PASEO_MODEL")
            .context("set PASEO_MODEL to a model ID discovered for codex")?;
        let created = client.create_agent(json!({
            "config": {"provider": "codex", "model": model, "cwd": cwd},
            "initialPrompt": "Review the current diff and identify its greatest risk."
        }), RequestOptions::default()).await?;
        let id = created["agent"]["id"].as_str().context("missing agent ID")?;
        let outcome = client.wait_for_finish(id, RequestOptions {
            timeout: Duration::from_secs(300),
            ..RequestOptions::default()
        }).await?;
        println!("{}: {}", outcome["status"], outcome["lastMessage"]);
        Ok(())
    }.await;
    client.close().await;
    result
}
```

## 等待回复

创建参数直接采用协议 JSON：`config.provider` 是 Provider ID，`config.model` 是独立的模型 ID，`config.cwd` 是工作目录，`initialPrompt` 启动第一轮。返回的是响应 payload，而不是 Agent 句柄。保存 `agent.id` 以便后续调用。

`wait_for_finish` 返回包含 `status`、`lastMessage`、`final`、`error` 的 payload，或返回传输/RPC 错误。Agent/Provider 的轮次失败保留在返回的结果 payload 中：即使 Rust 结果为 `Ok`，也要检查 `status` 和 `error`。请求超时或取消仅停止等待，不会取消 Agent；参见[错误处理](reference.zh-CN.md#errors-and-cleanup)。

## 断开连接

工作结束后始终调用 `client.close().await`，包括上例所示的错误路径。丢弃客户端会取消传输，但显式关闭会等待任务退出。两者都不会归档 Agent。请显式归档自己拥有的临时资源。

使用 `cargo run` 运行应用；它会执行真实 Provider 工作，可能产生费用。这里提供的是源码示例，不代表已经验证了真实 daemon/Provider 运行。

## 接下来

- [Agent](agents.zh-CN.md)：后续消息、标签、父子关系、结构化输出。
- [工作区](workspaces.zh-CN.md)：放置、后台任务、终端。
- [事件](events.zh-CN.md)：持有式订阅与历史修复。
- [参考](reference.zh-CN.md)：认证、relay、SSH、协议 API。
