# SDK 事件

[English](events.md) · [环境配置](quickstart.zh-CN.md)

建立连接或调用 `subscribe()` 只附加传输监听器，不会请求 daemon 观察。将已连接客户端包装为 `Arc::new(client)`，再通过持有式观察方法获取服务器快照和订阅 ID。支持该功能的 daemon 为独立观察分配独立服务器 ID。

## 跟踪 Agent 状态

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;
use std::sync::Arc;

async fn watch(client: &Arc<DaemonClient>) -> Result<()> {
    let mut observation = client.observe_agents(json!({
        "filter": {"includeArchived": false}, "page": {"limit": 50}
    }), RequestOptions::default()).await?;
    println!("initial: {}", observation.snapshot);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                event = observation.recv() => println!("update: {}", event?),
                signal = tokio::signal::ctrl_c() => { signal?; break; }
            }
        }
        Ok(())
    }.await;
    let released = observation.release().await;
    result?;
    released
}
```

初始 payload 位于 `observation.snapshot`；之后 `recv()` 返回完整 session 消息，包含 `type` 和通常存在的 `payload`。按自己的 Agent ID 筛选条目/事件。RPC payload 与事件封装形状不同。有限缓冲接收器可能落后；错误不是空更新。继续前应刷新快照并修复缺失历史。显式 `release` 等待服务器清理；丢弃观察只会在运行时仍存在时尝试异步清理。

## 跟踪时间线事件

`client.observe_timeline(&[agent_id.to_owned()], RequestOptions::default()).await?` 返回持有式观察。根据 `type` 和 payload 处理 `agent_stream` 及时间线相关消息。流增量不一定是完整消息。保留条目/轮次身份；替换事件应替换原条目而不是重复追加，缓存失效时重新读取。辅助函数位于 `paseo_protocol::timeline`，客户端不会代管展示缓存。

## 获取时间线历史

```rust
use anyhow::Result;
use paseo_client::{CursorPages, DaemonClient, RequestOptions};

async fn history(client: &DaemonClient, agent_id: &str) -> Result<()> {
    let mut pages = CursorPages::timeline(agent_id, "before", 100)?;
    while let Some(page) = pages.next(client, RequestOptions::default()).await? {
        println!("{}", page);
    }
    Ok(())
}
```

`CursorPages::timeline` 支持 `before` 和 `after`；以页面顺序和不透明的起止游标为准。单页调用使用 `fetch_agent_timeline`，传入 `agentId`、`direction`、`limit` 及可选 `cursor`/`projection`。`fetch_agent_history` 与 `CursorPages::history(fields)` 使用独立历史端点，不能与时间线分页互换。历史读取是显式操作，订阅不会自动重放。

## 跟踪工作区更新

在 `Arc<DaemonClient>` 上调用 `observe_workspaces(json!({"filter": {"includeBackground": true}}), options)`。视图关闭时释放返回的观察。普通 `fetch_workspaces` 读取不会创建观察，除非显式要求订阅。

## 跟踪 Provider 目录变化

使用 `observe_events(json!({"events": ["providers_snapshot_update", "agent_permission_request", "agent_permission_resolved"]}), options)` 订阅，然后获取工作目录的 Provider 快照并消费更新。事件名称是协议值，不是 Rust 回调。权限请求 ID 来自 daemon 事件，不是新生成的请求 ID：

```rust
use anyhow::Result;
use paseo_client::DaemonClient;
use serde_json::json;

async fn deny(client: &DaemonClient, agent_id: &str, permission_request_id: &str) -> Result<()> {
    client.respond_to_permission(json!({
        "agentId": agent_id, "requestId": permission_request_id,
        "response": {"behavior": "deny", "message": "Not approved by the operator"}
    })).await
}
```

只有操作员或应用明确策略授权该精确请求后才能发送 allow 响应。收到权限请求本身不代表获准执行。

## 重连与旧版 daemon

`DaemonClient` 不会自动重连。`ReconnectingClient::connect` 管理重连；`connection(&cancel).await?` 获取当前 `Arc<DaemonClient>`。其 `observe(request, response, fields, options)` 返回 `RestoringSubscription`；`recv().await` 返回 `Option<Result<SubscriptionEvent>>`。重连或落后后，`SubscriptionEvent::Snapshot { payload, restored: true }` 替换基线，需要自行补齐历史。`SubscriptionEvent::Update` 是实时事件。调用 `ReconnectingClient::close(&mut self)` 前释放恢复式订阅。

重连不会盲目重放修改操作。持有式观察方法检查 daemon 的 `ownedSubscriptions` 能力，并要求服务器返回 `subscriptionId`。不支持的主机会得到明确的升级提示，不会隐式回退到旧版共享过滤器。原始 RPC 与向后可读的协议消息仍可使用，但不能使未公布的观察功能变得可用。使用持有式观察前请升级主机。
