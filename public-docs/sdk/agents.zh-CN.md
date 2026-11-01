# 在 SDK 中使用 Agent

[English](agents.md) · [环境配置](quickstart.zh-CN.md)

示例是使用已连接 `DaemonClient` 的异步函数，依赖见快速开始。JSON 字段使用协议名称，不是 Rust 结构体字段。SDK 不会为你创建本地 Agent 句柄或缓存属性。

## 运行初始提示词

调用 `create_agent(fields, RequestOptions)`，传入 `config: {provider, model, cwd}` 和 `initialPrompt`。省略提示词会创建空闲会话。响应 payload 包含 `agent`；保存其 `id` 和 `workspaceId`。协议支持 `config.title` 及顶层 `labels`、`env`、`attachments`、`images`、`outputSchema`。

## 为后续提示词保持会话

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};
use std::time::Duration;

async fn review(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    let created = client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd,
                   "title": "Checkout reviewer"},
        "labels": {"my-app-role": "reviewer"}
    }), RequestOptions::default()).await?;
    let id = created["agent"]["id"].as_str().context("missing agent ID")?;
    client.send_agent_message(json!({
        "agentId": id, "text": "Review the current diff."
    })).await?;
    client.wait_for_finish(id, RequestOptions {
        timeout: Duration::from_secs(300), ..RequestOptions::default()
    }).await
}
```

`send_agent_message` 等待投递确认，而非轮次完成；随后 `wait_for_finish` 等待当前轮次。检查 `status`（`idle`、`permission`、`error` 或 `timeout`）、`lastMessage` 和 `error`：Agent/Provider 轮次失败保留在结果 payload 中，传输/RPC 失败则返回 `Err`。等待超时不会取消 Agent。要停止它，显式调用 `cancel_agent(json!({"agentId": id}))`。活动轮次中发送消息时可设置 `activeTurnBehavior` 为 `steer` 或 `interrupt`；不要假定并发发送会创建独立轮次。

## 按标签查找 Agent

```rust
use anyhow::Result;
use paseo_client::DaemonClient;
use serde_json::json;

async fn inspect(client: &DaemonClient, id: &str) -> Result<()> {
    let state = client.fetch_agent(json!({"agentId": id})).await?;
    println!("{}", state);
    let matches = client.fetch_all_agents(json!({
        "filter": {"labels": {"my-app-role": "reviewer"}, "includeArchived": false},
        "page": {"limit": 50}
    })).await?;
    println!("{} matching agents", matches.len());
    let commands = client.list_commands(json!({"agentId": id})).await?;
    println!("{}", commands["commands"]);
    Ok(())
}
```

`fetch_all_agents` 遍历所有游标页。需要限制内存时使用 `CursorPages::agents`。标签由应用管理；共享 daemon 时请使用命名空间。设置 `filter.includeBackground: true` 可包含后台工作。

## 通过 ID 继续 Agent

若需要确认旧 ID 的存在性和最新状态，先调用 `fetch_agent`；不存在的资源不是空的本地句柄。然后使用该 ID 调用 `send_agent_message` 与 `wait_for_finish`。`resume_agent`、`import_agent` 是独立的 Provider 会话操作，读取 ID 不会隐式调用它们。

## 创建子 Agent

向 `create_agent` 传入 `callerAgentId`、`workspaceId`，且 `config.cwd` 必须匹配工作区实际目录。读取父 Agent 最新快照，不要猜测位置。归档父 Agent 会级联归档子 Agent；若子 Agent 需要独立保留，请先调用 `detach_agent(json!({"agentId": child_id}))`。

## 在后台工作区中运行工作

调用 `create_workspace`，在顶层设置 `background: true`、`source` 及可选的 `agent: {config, initialPrompt}`，参见[工作区创建](workspaces.zh-CN.md)。不要把后台可见性放进嵌套 Agent。Agent 保留普通生命周期和历史。默认列表隐藏后台工作区，需显式包含；已知 Agent ID 仍可正常使用。

## 请求结构化输出

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn assess(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd},
        "outputSchema": {
            "type": "object",
            "properties": {"summary": {"type": "string"},
                           "risk": {"type": "string", "enum": ["low", "medium", "high"]}},
            "required": ["summary", "risk"], "additionalProperties": false
        },
        "initialPrompt": "Assess the release risk of the current diff."
    }), RequestOptions::default()).await
}
```

成功等待 Agent 后，使用 `serde_json::from_str::<Value>` 解析 `lastMessage`。信任结果前检查状态、字段存在性、schema 和应用约束。不能把不支持结构化输出的 Provider 当作已经替应用完成验证。

## 读取 Agent 的当前状态

`fetch_agent` 返回最新快照。Agent 字段包括 `status`、`capabilities`、`availableModes`、`pendingPermissions`、`activeTurn`、`lastUsage`、`lastError`、`features`、`runtimeInfo`、`archivedAt`、`workspaceId`、`cwd`。字段可能缺失或为 null。Rust JSON 下标读取缺失字段会得到 null；使用 `get`/`as_*`，对必填字段损坏报错，不要静默使用默认值。持续更新见[持有式观察](events.zh-CN.md)。

## 列出会话加载的命令

`list_commands` 读取运行中的 Provider 会话，也包括磁盘上不存在的内置命令/技能。`kind` 可选；缺失不代表 `command`。不支持的 Provider 可返回错误，SDK 将其暴露为 `ClientError`，原始响应保留在 `details`。

## 归档或解除父子关系

`archive_agent(json!({"agentId": id}))` 软删除 Agent 并关闭运行时。`detach_agent` 仅解除父子关系。`delete_agent` 是另一个破坏性操作。关闭 SDK 连接不会归档任何资源。仅清理自己拥有的资源，包括错误路径；参见[实践示例](recipes.zh-CN.md)。
