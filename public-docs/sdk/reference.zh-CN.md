# SDK API 参考

[English](reference.md) · [依赖与 Tokio 配置](quickstart.zh-CN.md)

Rust 公开入口为 `paseo_client`、`paseo_protocol`、`paseo_highlight`。参见[客户端源码](../../packages/client/src/lib.rs)、[操作映射](../../packages/client/src/operations.rs)、[订阅类型](../../packages/client/src/subscriptions.rs)。JSON 参数与响应 payload 使用 `serde_json::Value`，保留 Provider/插件扩展字段。不存在隐式 Agent/工作区门面对象。

## 连接与认证

```rust
use anyhow::Result;
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget};
use tokio_util::sync::CancellationToken;

async fn connect_remote() -> Result<DaemonClient> {
    let host = std::env::var("PASEO_HOST")?;
    let mut options = ConnectOptions::new(DaemonTarget::Endpoint { host });
    options.password = std::env::var("PASEO_PASSWORD").ok();
    DaemonClient::connect(options, CancellationToken::new()).await
}
```

`DaemonTarget::Instance { home: PathBuf }` 发现本地 daemon。`DaemonTarget::Endpoint { host: String }` 支持：

| 目标 | 示例 / 含义 |
| --- | --- |
| 直连 WebSocket | `wss://daemon.example/ws` 或回环 `ws://127.0.0.1:6767/ws` |
| 主机/端口 | `127.0.0.1:6767`、`tcp://127.0.0.1:6767` |
| Unix socket | `unix:///absolute/path/to/daemon.sock` |
| Windows pipe | `pipe://` 后接实际命名管道路径 |
| SSH | `ssh://user@host:22?daemonPort=6767` |
| Relay | daemon 生成的 `relay://` URL 或带 `#connect=` / `#offer=` 的配对 URL |
| 已注册主机 | 通过本地管理 daemon 解析的 `hst_…` 或 `srv_…` |

原样使用 daemon 生成的配对材料，不要编造公钥。Relay URL 携带 daemon 公钥固定值；加密直连设置 `ConnectOptions.server_public_key`。`minimum_security: paseo_client::crypto::SecurityMode::Protected` 要求受保护的加密记录，不会启用 TLS，也不替代公钥固定/密码。`wss` 验证 TLS 端点。不要通过不可信明文 `ws` 发送 daemon 密码。

`ConnectOptions::new(target)` 默认连接超时 15 秒、`client_type: "cli"`、`SecurityMode::Legacy`。可选 `password` 覆盖 URL 认证，否则可从 `PASEO_PASSWORD` 读取。`local_credential` 显式指定本地凭据。自动本地凭据仅用于选定本地 daemon 公布的精确端点，绝不发给任意远端。`home` 为显式端点指定身份/发现存储目录；`client_id` 覆盖通常持久化的 SDK 身份。不要在日志中泄露密码、凭据文件、配对 URL 或密钥。

SSH 使用本机 `ssh` 可执行文件、非交互批量认证及指向远端回环的 `-W` 转发。事先配置密钥/agent 和可信主机密钥，不会启动远端 daemon。`password` 是 daemon 认证，不是 SSH 登录密码。SSH 配置和主机验证由 SSH 客户端负责。

## 客户端生命周期

| 公开 API | 结果 / 行为 |
| --- | --- |
| `DaemonClient::connect(options, cancel).await` | 已连接的 `Result<DaemonClient>`，执行握手/认证。 |
| `server_info()` | 当前 JSON 服务端能力/元数据快照。 |
| `require_feature(key, message)` | 显式拒绝不支持的能力。 |
| `subscribe()`、`subscribe_binary()` | 本地有限缓冲接收器，不创建服务器观察。 |
| `send(message).await`、`send_binary(bytes).await` | 发送 session JSON 或二进制帧，不处理 RPC 结果。 |
| `request(message, response_type, timeout).await` | 关联 RPC，缺少请求 ID 时自动生成。 |
| `rpc(request_type, response_type, fields, options).await` | 显式超时/取消的关联 JSON 操作。 |
| `flush().await` | 等待出站传输 flush。 |
| `cancellation()` | 连接生命周期取消 token。 |
| `close().await` | 取消并等待自有任务，不归档 Agent/工作区。 |

`RequestOptions { timeout, cancel }` 默认 30 秒与新取消 token。请求超时为零表示无期限。`request_cancellable` 直接接受 token。已提交的修改可能在超时、取消、断线后完成；丢弃 future 不代表回滚。

## Agent 与工作区

`create_agent(fields, options)` / `create_workspace(fields, options)` 要求带 key 的创建生命周期能力，并保留/生成 `idempotencyKey`。返回包含资源的 payload，而非句柄。需要恢复时保留显式 key。普通命名方法接受单个 JSON `fields` 参数，使用默认请求选项；`update_agent(agent_id, patch)` 例外，单独接受 ID。

| 领域 | 方法 |
| --- | --- |
| Agent 查询 | `fetch_agent`、`fetch_agents`、`fetch_all_agents`、`fetch_agent_history`、`fetch_agent_timeline`、`list_commands` |
| 轮次 | `send_agent_message`、`wait_for_finish(agent_id, options)`、`cancel_agent`、`rewind_agent` |
| 配置 | `update_agent`、`set_agent_mode`、`set_agent_model`、`set_agent_feature`、`set_agent_thinking_option`、`apply_agent_config` |
| 生命周期 | `resume_agent`、`import_agent`、`refresh_agent`、`archive_agent`、`detach_agent`、`delete_agent` |
| 子 Agent | `list_provider_subagents`、`fetch_provider_subagent_timeline`、`build_agent_fork_context` |
| 项目 | `open_project`、`add_project`、`list_projects`、`rename_project`、`set_project_icon`、`remove_project` |
| 工作区 | `fetch_workspaces`、`archive_workspace`、`set_workspace_title`、`set_workspace_pinned`、`inspect_workspace_recovery`、`restore_workspace` |
| 初始化/脚本 | `fetch_workspace_setup_status`、`run_workspace_setup`、`list_workspace_scripts`、`start_workspace_script_with_status`、`stop_workspace_script` |
| 标签 | `list_workspace_labels`、`set_workspace_label`、`update_workspace_label`、`delete_workspace_label` |

参数见 [Agent](agents.zh-CN.md)、[工作区](workspaces.zh-CN.md)及[完整请求 schema](../../packages/protocol/schema/session-inbound.json)。`CursorPages::{agents,workspaces,history}` 和可失败的 `CursorPages::timeline` 提供 `next(&client, options).await -> Result<Option<Value>>`；重复游标是错误。除非调用者收集，否则不会累计所有页面。

## 事件与恢复

持有式观察要求 `Arc<DaemonClient>`：`observe_agents`、`observe_workspaces`、`observe_events`、`observe_timeline`、`observe_terminals`、`observe_terminal`。`OwnedSubscription` 提供 `snapshot`、`subscription_id`、`recv().await`、`release().await`。当前方法要求 `ownedSubscriptions`，不模拟旧版共享过滤订阅。事件仍是完整 session 消息。

`ReconnectingClient` 恢复显式持有的观察，不重放任意修改。`connection(&cancel)` 返回当前 `Arc<DaemonClient>`。`RestoringSubscription` 返回 `SubscriptionEvent::{Snapshot,Update}`；恢复快照使旧基线失效，需要补齐历史。`recover_creation(kind, key, fields, options)` 使用原始 key 显式恢复不确定创建。参见[事件](events.zh-CN.md)、[实践示例](recipes.zh-CN.md)。

<a id="terminals-and-files"></a>
## 终端与文件

`create_terminal`、`list_terminals`、`rename_terminal`、`capture_terminal`、`kill_terminal` 管理 daemon PTY。包括工作区身份和实际目录。`observe_terminal(id, restore, options)` 返回 `TerminalStream`；`recv` 产生 `TerminalEvent::{Frame,Exit}`，`input` 发送字节，`resize` 接受 `paseo_protocol::binary_frames::TerminalResize`。显式释放 `stream.subscription`。这些 API 不会为 Flutter 添加交互终端 UI。

`read_file(cwd, path, max_bytes, options)` 返回有字节上限的 `DownloadedFile { metadata, bytes }`。`upload_file(file_name, mime, modified_at, bytes, options)` 使用有限大小二进制帧。`list_directory`、`write_file`、`create_file_entry`、`rename_file_entry`、`duplicate_file_entry`、`delete_file_entry` 提供文件操作；破坏性调用前确认准确 schema。上传取消不保证回滚或自动重试。

## Provider、配置与连接主机

Provider 方法：`list_available_providers`、`get_providers_snapshot`、`refresh_providers_snapshot`、`list_provider_models`、`list_provider_modes`、`list_provider_features`、`get_provider_diagnostic`、`list_provider_usage`、`list_usage_reports`。压缩快照和草稿配置见 [Provider](providers.zh-CN.md)。

配置方法为 `get_daemon_config`、`patch_daemon_config`、`read_project_config`、`write_project_config`；`get_daemon_status` 和能力检查后的 `daemon_status(timeout)` 读取状态。诊断输出不得泄露秘密。`restart`、`shutdown`、`update_daemon`、`reload_daemon_config` 等管理操作影响所选正在运行的 daemon，必须有操作员明确意图。`features.durableDaemonUpdates` 为 true 时，`submit_daemon_update`（调用方生成 UUID `operationId`，仅限官方发布版）返回已持久接受的 `operation`，`get_daemon_update_status` 按 `operationId` 读取。确认丢失后用同一 `operationId` 重新提交会返回同一操作；断开连接不会取消它。`phase` 为 `committed`、`rolled_back` 或 `failed` 才是更新结果，`continuation.state` 单独报告请求的会话恢复。旧版 `update_daemon` 使用同一引擎，仅在操作 committed 时报告 `success: true`。这类 daemon 还接受在 `session.events.set_subscription` 中订阅 `daemon.update.maintenance` 事件（旧 daemon 会拒绝未知事件名，因此仅在声明该能力时订阅）。其 payload `{operationId, active}` 表示更新何时暂停新消息和新建 Agent；`server_info.daemonUpdate`（`{operationId, active: true}`）在每次连接时携带当前暂停状态，否则不存在。仅当 `active: false` 指向你正在显示的操作时才清除提示。

连接主机使用 `list_hosts`、`add_host`、`update_host`、`remove_host`、`open_host_route`、`open_host_link`。把注册 ID 用作 `DaemonTarget::Endpoint` 即可使用管理路由。管理 daemon 和目标 daemon 都必须支持所需能力。路由不隐含管理其他 daemon 的授权。底层路由元数据/帧编解码位于 `paseo_protocol::{routing,binary_frames}`。

操作模块还提供 checkout/forge、调度、插件、用量和浏览器主机注册 RPC，保留协议而不创建额外本地运行时。无需命名便利方法时可用 `rpc` 显式指定请求/响应对。

<a id="errors-and-cleanup"></a>
## 错误与清理

```rust
use anyhow::Result;
use paseo_client::{ClientError, DaemonClient};
use serde_json::json;

async fn inspect_errors(client: &DaemonClient, id: &str) -> Result<()> {
    match client.fetch_agent(json!({"agentId": id})).await {
        Ok(payload) => println!("{}", payload),
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<ClientError>() {
                eprintln!("daemon code: {}", failure.code);
                // Inspect failure.details deliberately; it may contain private data.
            }
            return Err(error);
        }
    }
    Ok(())
}
```

方法返回 `anyhow::Result`，并非所有错误都是 `ClientError`。该类型提供 `code`、`message` 和可选 JSON `details`。传输/请求代码包括 `DAEMON_UNREACHABLE`、`DAEMON_CONNECTION_LOST`、`DAEMON_REQUEST_TIMEOUT`、`DAEMON_REQUEST_CANCELLED`、`DAEMON_RPC_ERROR`；目标选择也报告歧义、无效或仅限本地错误。Daemon 拒绝 payload 可能包含更具体的代码。

RPC payload 中的字符串 `error`、`accepted: false` 或 `success: false` 通常作为错误暴露，原始 payload 保留在 `ClientError.details`。`wait_for_finish_response` 则不同：其中字符串 `error` 描述 Agent 轮次结果，保留在返回 payload 中。即使 Rust 结果为 `Ok`，也必须检查结果的 `status` 和 `error`；`permission`/`timeout` 不是完成。真正的传输/RPC 失败仍返回 `Err`。失败时需释放订阅并关闭客户端。只归档自己拥有的资源，关闭传输不替代归档。

## 协议 API

`paseo_protocol::wire::{Direction,SessionMessage,Envelope,parse_envelope,encode_envelope,validate_session}` 验证原生 JSON schema。`Inbound` 表示**发往 daemon**，`Outbound` 表示**来自 daemon**。`SessionMessage::parse` 验证且不剥除未知扩展字段；`new` 和普通反序列化不执行完整 schema 验证。协议 crypto 负责传输加密，不负责应用密码授权。权威 schema 维护与示例见[协议验证](../../docs/protocol-validation.md)。

## 语法高亮

在快速开始应用依赖中加入 `paseo-highlight = { path = "../paseo/packages/highlight" }`。原生语法除 Rust/Cargo 外还需要平台 C 编译器。独立同步示例：

```rust
use paseo_highlight::{ColorScheme, SyntaxThemeId, highlight_code, resolve_syntax_colors};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = "fn main() { println!(\"hello\"); }";
    let colors = resolve_syntax_colors(SyntaxThemeId::Github, ColorScheme::Dark);
    for line in highlight_code(source, "example.rs")? {
        for token in line {
            let color = token.style.map(|role| colors.color(role));
            println!("{:?}: {:?}", color, token.text);
        }
    }
    Ok(())
}
```

`highlight_code`/`highlight_line` 从输入借用 token 文本。重复工作可复用 `Highlighter::new()` 及其方法。`get_language_for_file`、`is_language_supported`、`get_supported_extensions`、`get_parser_for_file` 提供语言查询；未知扩展名视为纯文本，受支持解析器失败则报错。`StyledRange` 使用 UTF-8 字节偏移，`byte_to_utf16` 和 `utf16_to_byte` 可适配其他消费者。跨行语法应高亮完整文档。

`highlight_code_in_steps(code, filename, HighlightStepsOptions)` 启动原生工作线程。`HighlightSteps` 支持阻塞迭代或非阻塞 `poll_next`。`HighlightStep::Preview` 是暂定结果，最终 `Lines { first_line, lines }` 替换对应位置，`Finished` 表示完成。这是有界分批交付，不是编辑增量解析器。丢弃句柄会取消交付，但进行中的原生解析可能完成。

`SyntaxThemeId` 包括 Github、Catppuccin、Dracula、TokyoNight、One、Nord、Gruvbox、Solarized。`resolve_syntax_colors` 选择明暗配色（Dracula 共用一种）；`create_highlight_style` 为 20 个 `HighlightStyle` 角色生成原生规则。调用者负责渲染，无需浏览器编辑器或框架运行时。
