# SDK 实践示例

[English](recipes.md) · [环境配置](quickstart.zh-CN.md)

以下异步函数使用已连接客户端及快速开始中的依赖，会执行真实 Provider 工作。传入已发现的模型 ID 和 daemon 主机目录。调用者应在函数返回后关闭连接，包括失败路径。

## 把 issue 变成可见工作

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;

async fn start_issue(client: &DaemonClient, cwd: &str, model: &str,
                     issue_id: &str, title: &str, description: &str) -> Result<(String, String)> {
    let opened = client.open_project(json!({"cwd": cwd})).await?;
    let workspace = &opened["workspace"];
    let workspace_id = workspace["id"].as_str().context("missing workspace ID")?;
    let directory = workspace["workspaceDirectory"].as_str().context("missing directory")?;
    let created = client.create_agent(json!({
        "workspaceId": workspace_id,
        "config": {"provider": "codex", "model": model, "cwd": directory, "title": title},
        "labels": {"my-tracker/issue-id": issue_id},
        "initialPrompt": format!("Implement issue {issue_id}: {title}\n\n{description}")
    }), RequestOptions::default()).await?;
    let agent_id = created["agent"]["id"].as_str().context("missing agent ID")?;
    Ok((workspace_id.to_owned(), agent_id.to_owned()))
}
```

在集成中保存两个 ID。后续 webhook 应读取并复用已有资源，不要盲目再次创建。Issue 文本是不可信任务内容，不授权无关主机操作。打开的工作区可能早已存在，不能因为本函数使用过它就归档。

## 运行并行审查者

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};
use std::time::Duration;

async fn reviewers(client: &DaemonClient, cwd: &str, model: &str) -> Result<()> {
    let mut owned = Vec::<String>::new();
    let work: Result<()> = async {
        for prompt in ["Review correctness and edge cases.", "Review security and input handling."] {
            let created = client.create_agent(json!({
                "config": {"provider": "codex", "model": model, "cwd": cwd},
                "initialPrompt": prompt
            }), RequestOptions::default()).await?;
            owned.push(created["agent"]["id"].as_str().context("missing agent ID")?.to_owned());
        }
        let options = RequestOptions { timeout: Duration::from_secs(300), ..RequestOptions::default() };
        let (first, second) = tokio::join!(
            client.wait_for_finish(&owned[0], options.clone()),
            client.wait_for_finish(&owned[1], options)
        );
        for outcome in [first, second] {
            let outcome: Value = outcome?;
            println!("{}", outcome);
            anyhow::ensure!(outcome["status"] == "idle", "review did not complete: {outcome}");
        }
        Ok(())
    }.await;
    let mut cleanup_error = None;
    for id in owned {
        if let Err(error) = client.archive_agent(json!({"agentId": id})).await {
            eprintln!("archive failed for {id}: {error}");
            if cleanup_error.is_none() { cleanup_error = Some(error); }
        }
    }
    work?;
    if let Some(error) = cleanup_error { return Err(error); }
    Ok(())
}
```

两个 Agent 在创建时就开始工作，等待操作并发执行。每次创建返回的 ID 会在下一次创建前保存，因此部分失败仍能清理先前 Agent。若提交后创建超时，资源可能已存在但没有返回 ID；应保存调用者提供的 `idempotencyKey`，恢复结果后再判断清理是否完成。重试时不要换新 key。

## 跨进程重启保留常驻角色

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;

async fn planner(client: &DaemonClient, cwd: &str, model: &str) -> Result<String> {
    let found = client.fetch_all_agents(json!({
        "filter": {"includeArchived": false, "includeBackground": true,
                   "labels": {"my-app/role": "planner"}},
        "page": {"limit": 100}
    })).await?;
    anyhow::ensure!(found.len() <= 1, "multiple planners; resolve ownership explicitly");
    if let Some(agent) = found.first() {
        return Ok(agent["id"].as_str().context("missing agent ID")?.to_owned());
    }
    let created = client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd, "title": "Planner"},
        "labels": {"my-app/role": "planner"}
    }), RequestOptions::default()).await?;
    Ok(created["agent"]["id"].as_str().context("missing agent ID")?.to_owned())
}
```

使用返回 ID 调用 `send_agent_message`、`wait_for_finish`。为标签设置命名空间，并在集成中串行化查找/创建操作；标签不是唯一性锁。若多个进程可能创建同一角色，应协调持久创建 key/锁，不能假定列表与创建是原子操作。按实际主机/项目归属筛选，不要随意选择第一个无关匹配 Agent。

## 清理临时 Agent

审查示例即使工作或其他清理失败，也尝试所有归档。关闭连接与清理资源是两回事。若清理需跨进程崩溃保留，应持久化 ID。只归档自己拥有的临时 Agent；常驻角色和原有工作区应保留。

对于结果不确定的创建，`ReconnectingClient::recover_creation(kind, key, original_fields, options)` 检查带 key 的生命周期，仅在快照缺失时重新提交。保留原始字段和 key。恢复可能返回创建生命周期快照或创建 RPC payload，两者路径不一定相同，需检查形状。其他修改不会自动重试；主动重试前先检查当前状态。
