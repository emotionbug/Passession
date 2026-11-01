# Provider 选项

[English](provider-options.md) · [环境配置](quickstart.zh-CN.md)

Provider 选项是由 Provider 解释的 JSON 对象（`serde_json::Value`）。Claude、Codex、OpenCode 验证接受的键，未知或拼错的键可能导致创建失败。Pi、OMP、ACP 和原生插件 Provider 解释各自选项，不能假定所有 Provider 使用同一种策略。

默认值位于 `config.json` 的 `agents.providers.<id>.options`。Rust 协议形状的创建请求把单个 Agent 覆盖值放在 **`config.providerOptions`**，不是 `config.options`。Daemon 递归合并普通对象；数组、标量、null 替换基础值。只保存 Agent 自身覆盖值，默认值变更在下一次启动/恢复时生效。

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn launch(client: &DaemonClient, provider: &str, model: &str,
                cwd: &str, provider_options: Value) -> Result<Value> {
    client.create_agent(json!({
        "config": {"provider": provider, "model": model, "cwd": cwd,
                   "providerOptions": provider_options},
        "initialPrompt": "Review the change and report the result."
    }), RequestOptions::default()).await
}
```

将下面的 JSON 对象传给 `launch`，例如通过 `serde_json::json!` 构造，或使用 `serde_json::from_str` 解析配置字符串。使用该 Provider 实际发现的模型 ID。再用返回的 Agent ID 调用 `wait_for_finish` 观察结果。

这些选项限制的是以 daemon 用户身份运行的外部 CLI，不是主机安全边界。不可信工作应在容器或独立主机执行，参见[安全](../security.zh-CN.md)。

## Codex

```json
{
  "approval_policy": "never",
  "sandbox_mode": "workspace-write",
  "sandbox_workspace_write": {
    "writable_roots": ["/home/me/dev/storefront"],
    "network_access": false,
    "exclude_slash_tmp": true
  },
  "web_search": "disabled"
}
```

| 选项 | 接受的值/字段 |
| --- | --- |
| `approval_policy` | `untrusted`、`on-request`、`never` 或 `granular` 策略对象 |
| `sandbox_mode` | `read-only`、`workspace-write`、`danger-full-access` |
| `sandbox_workspace_write` | `writable_roots`、`network_access`、`exclude_slash_tmp`、`exclude_tmpdir_env_var` |
| `web_search` | `disabled`、`cached`、`indexed`、`live` |
| `features` | `multi_agent_v2`、`network_proxy`（布尔值或代理/域名策略对象） |

`approval_policy: "never"` 仅取消询问，不限制访问。必须明确配置沙箱。例如默认 `sandbox_mode: "read-only"`、`approval_policy: "never"`、`sandbox_workspace_write.network_access: false`，Agent 仅覆盖 `sandbox_mode` 为 `workspace-write` 时，后两个默认值仍保留。

## Claude

```json
{
  "disallowedTools": ["WebFetch"],
  "sandbox": {
    "enabled": true,
    "failIfUnavailable": true,
    "allowUnsandboxedCommands": false,
    "filesystem": {
      "allowWrite": ["/home/me/dev/storefront"],
      "denyRead": ["/home/me/.ssh", "/home/me/.aws"]
    },
    "network": {
      "allowedDomains": ["crates.io", "github.com"],
      "strictAllowlist": true
    }
  }
}
```

| 选项 | 含义 |
| --- | --- |
| `allowedTools`、`disallowedTools` | 无需询问可用的工具 / 禁止的工具 |
| `additionalDirectories` | 额外可访问目录 |
| `sandbox` | `enabled`、`failIfUnavailable`、`allowUnsandboxedCommands`、`excludedCommands`、`filesystem`、`network` |
| `settings` | 包含 allow/ask/deny 规则列表的 `permissions`，以及嵌套沙箱设置 |

`failIfUnavailable: true` 在无法建立沙箱时拒绝启动；关闭它允许回退为无沙箱运行。文件系统设置包括 `allowWrite`、`denyWrite`、`allowRead`、`denyRead`；网络设置包括域名允许/拒绝列表、`strictAllowlist` 和代理。根据 daemon 主机与真实任务调整路径和域名。

## OpenCode

```json
{
  "permission": {
    "read": "allow", "edit": "allow", "webfetch": "deny",
    "external_directory": "deny",
    "bash": {"git status": "allow", "git diff*": "allow", "git push*": "deny", "*": "ask"}
  }
}
```

动作是 `ask`、`allow`、`deny`。带目标的工具（`read`、`edit`、`glob`、`grep`、`list`、`bash`、`task`、`external_directory`、`repo_clone`、`repo_overview`、`lsp`、`skill`）也接受模式映射。模式顺序及回退条目很重要。不带目标的工具（`todowrite`、`question`、`webfetch`、`websearch`、`codesearch`、`doom_loop`）使用单一动作。单独设置 `permission: "deny"` 拒绝全部操作。

`ask` 会变成 Paseo 权限请求。通过持有式事件观察和 `respond_to_permission` 处理，参见[事件](events.zh-CN.md)。等待结果可能为 `permission`。不要为了无人值守而自动允许所有请求。

## 模式与选项

`config.modeId` 选择 [Provider 发现](providers.zh-CN.md)中的模式，`config.providerOptions` 是 Provider 原生配置，两者可同时设置。冲突时显式 Provider 选项优先。例如 `modeId: "full-access"` 加上 Codex `providerOptions: {"approval_policy":"never","sandbox_mode":"read-only"}` 仍是只读。不要仅从模式标签推断权限，应检查具体选项。
