# 在 SDK 中使用工作区

[English](workspaces.md) · [环境配置](quickstart.zh-CN.md)

工作区为 Agent、脚本/初始化 PTY 和文件提供持久归属。SDK 可以控制 daemon 保留的终端；原生应用以 **Terminal** 标签页打开交互式 shell，并以只读任务日志显示脚本/初始化输出。以下路径均属于 daemon 主机。

## 打开目录

`client.open_project(json!({"cwd": directory})).await?` 打开项目目录并复用该精确目录的活动工作区。检查响应 payload，不要假定存在本地工作区句柄。它与请求新工作区的 `create_workspace` 不同。

## 创建新工作区

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn create_work(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    client.create_workspace(json!({
        "source": {"kind": "directory", "path": cwd},
        "title": "Checkout review",
        "background": true,
        "agent": {
            "config": {"provider": "codex", "model": model, "cwd": cwd},
            "initialPrompt": "Map the checkout flow before changing anything."
        }
    }), RequestOptions::default()).await
}
```

响应包含 `workspace`、可选 `agent` 和初始化/创建元数据，请保存 ID。若需要隔离检出，将 `source` 替换为 `{"kind":"worktree","cwd":cwd,"action":"branch-off","refName":"main","branchName":"fix/checkout-42"}`。目录和 worktree 来源均可携带 `projectId`，否则由 daemon 解析项目。Worktree 创建会确定实际目录；后续操作应使用返回的工作区目录，而非源仓库路径。

`background` 属于工作区。省略时默认 false；提供 `callerAgentId` 时继承调用者工作区可见性。显式 true/false 覆盖继承。它仅用于创建；向已有工作区添加 Agent 不会改变可见性。嵌套的 `agent.background` 会被拒绝。通过 `create_agent` 隐式创建工作区时，只能在未指定已有 `workspaceId` 时使用顶层 `background`。

请求后台创建或包含后台资源的发现前，调用 `client.require_feature("backgroundWorkspaces", "Update the host for background workspaces")?`。不要静默降级为可见工作。

## 在工作区启动 Agent

调用 `create_agent`，提供 `workspaceId` 和 `config: {provider, model, cwd}`，其中目录必须是工作区实际目录。不提供 `workspaceId` 时，创建 Agent 会按需建立项目和新工作区。原始 JSON API 不会像本地句柄那样代填位置字段。

## 在工作区启动终端

```rust
use anyhow::{Context, Result};
use paseo_client::DaemonClient;
use serde_json::json;

async fn run_log(client: &DaemonClient, workspace_id: &str, cwd: &str) -> Result<()> {
    let created = client.create_terminal(json!({
        "workspaceId": workspace_id, "cwd": cwd, "name": "Repository status",
        "command": "git", "args": ["status", "--short"],
        "size": {"rows": 24, "cols": 100}
    })).await?;
    let id = created["terminal"]["id"].as_str().context("missing terminal ID")?;
    let capture = client.capture_terminal(json!({"terminalId": id, "stripAnsi": true})).await;
    let cleanup = client.kill_terminal(json!({"terminalId": id})).await;
    println!("{}", capture?);
    cleanup?;
    Ok(())
}
```

Capture 是某一时刻的快照，不会等待进程退出。需要等待输出/退出时，使用 `Arc<DaemonClient>::observe_terminal` 接收 `TerminalEvent`，并释放其订阅；参见[参考](reference.zh-CN.md#terminals-and-files)。始终传入 `workspaceId`，以区分共享目录的工作区。`list_terminals` 接受相同位置上下文。通过 `TerminalStream::input` 写入输入；可执行文件与参数分开传递，切勿将不可信文本拼接进 shell 命令。

## 列出工作区

```rust
use anyhow::Result;
use paseo_client::{CursorPages, DaemonClient, RequestOptions};
use serde_json::json;

async fn list_work(client: &DaemonClient) -> Result<()> {
    let mut pages = CursorPages::workspaces(json!({
        "filter": {"query": "storefront", "includeBackground": true},
        "page": {"limit": 50}
    }));
    while let Some(page) = pages.next(client, RequestOptions::default()).await? {
        println!("{}", page["entries"]);
    }
    Ok(())
}
```

除非 `filter.includeBackground` 为 true，否则不显示后台工作区。`fetch_workspaces` 返回单次 payload；`CursorPages::workspaces` 遍历所有页面并返回每页对象。游标是不透明值。

## 刷新与归档

Rust 不提供需刷新的本地句柄。使用 `fetch_workspaces` 读取最新数据，或持有[工作区观察](events.zh-CN.md)。使用 `archive_workspace(json!({"workspaceId": id})).await?` 显式归档。工作区归档不同于 Agent 归档。通过 `inspect_workspace_recovery` 检查恢复状态；仅当应用确实要恢复时调用 `restore_workspace`。只归档自己拥有的资源。
