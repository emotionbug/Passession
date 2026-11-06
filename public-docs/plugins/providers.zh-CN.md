---
title: 构建 Provider 插件
description: 通过原生 Rust Provider 协议接入真实 Agent。
nav: Provider 插件
---

# 构建 Provider 插件 {#build-a-provider-plugin}

从[快速上手](index.zh-CN.md)开始。本文针对尚未发布的 **0.13.1 源码检出**。Provider 在 daemon 主机的原生可执行文件中运行；原生客户端渲染协议数据，不运行插件 React 组件。现有第一方原生 Provider 仍为内置功能；Kiro 保持禁用，开发插件不会启用它。

完整可运行示例是 [workspace-inspector](../../plugin-examples/workspace-inspector/src/lib.rs)，crate 名为 `paseo-workspace-inspector`。它实现经权限批准的本地 UTF-8 文件统计，不是 AI/模型服务。[快速上手命令](index.zh-CN.md#run-the-repository-example)提供构建、校验和安装步骤。示例有意不声明持久化、转向、配置变更、图片或输出 schema。应参考其真实注册/连接/会话流程，而不是认为它支持所有可选能力。

## 注册直接实现的 Provider {#register-a-direct-provider}

实现公开 [Provider trait](../../packages/plugin/src/provider.rs)：

- `Provider::metadata(&self) -> Metadata`：使用 `Metadata::new(id, label)`，设置适用的描述、`icon_path`、命令及可选能力标志。
- `Provider::connect(&self, request: Value, events: Events, context: Context) -> Result<Arc<dyn Connection>>`：实现上使用 `#[paseo_plugin::async_trait]`，从 `request.versions`/`request.capabilities` 协商版本和能力子集。
- `Connection::version() -> u64` 与 `capabilities() -> Vec<String>` 返回协商结果；运行时拒绝宿主未提供的值。
- `Connection::send(input: Value) -> Result<()>` 接受输入；结果与会话变化通过 `events.emit(event).await` 发送，不是 `send` 返回值。
- `Connection::close() -> Result<()>` 结束所拥有的会话、生产者任务和子进程，解决待处理权限。`Events::cancellation()` 通知连接关闭，并可中断阻塞的事件发送。

在 `plugin.run().await` 前调用 `plugin.provider(implementation)?`。可选 `Provider::status` 与 `catalog_cache_key` 必须和 `Metadata::has_status`、`has_catalog_cache_key` 一致。元数据应确定性生成，让校验和就绪描述相同贡献。JSON 保留宿主 wire 形状；必须校验，Rust `Value` 本身不强制 schema。

## 启动 Provider CLI {#launch-the-provider-cli}

外部 Agent CLI 是独立产品。使用 `tokio::process::Command` 等 Rust 进程 API，分别传入参数、工作目录，以及 `request.launch` 提供的命令、参数前缀和环境。明确传递解析后的环境，避免意外继承无关凭据，也不要把启动覆盖值当成完整环境。不要用解释器脚本包装 Paseo 插件；`server.executable` 必须仍是原生二进制。

直接实现 `Provider`/`Connection` 时，自行负责平台启动差异、进程树清理和有界等待；旧 JavaScript 的 `spawnProcess`/`execCommand`/`terminateProcess` 函数不是 Rust API。关闭 stdin 请求优雅退出，必要时终止自有进程，不要杀死无关进程。ACP Agent 应使用负责传输与会话生命周期的[原生 ACP 适配器](#adapt-an-acp-agent)。披露所需外部软件，下载/安装前获得用户同意。

## 返回模型、模式和思考选项 {#return-models-modes-and-thinking-options}

`catalog` 输入包含 `requestId`；返回同 ID 的 `catalog` 事件及 `catalog` 对象。模型/模式使用稳定 ID 和标签，可选思考选项与默认值应符合当前协议。不支持的类别返回空项，不虚构选项。示例真实响应为：

```json
{"type":"catalog","requestId":"request-1","catalog":{"models":[{"id":"local-file","label":"Local text files"}],"modes":[],"defaultModel":"local-file"}}
```

目录在会话创建前可用，会话专属配置稍后通过 `session.config` 提供。完整形状见[宿主 Provider 桥接](../../packages/daemon/src/plugins/provider.rs)和 [SDK Provider 选项](../sdk/provider-options.zh-CN.md)。

## 打开会话 {#open-a-session}

`session.open` 包含 `sessionId`、`requestId`、启动配置、可选持久化数据和历史模式。`config` 包括工作目录、模型/模式/思考、环境、系统提示、MCP 服务器、工具策略和不透明 Provider 选项。校验私有选项，并与用户可见设置区分。通过 `Settings::read` 读取持久设置，缓存时订阅变更。

创建/恢复实际底层会话后，发送匹配 ID 的 `session.opened`，包含实际 `cwd`、能力和恢复策略；适用时发送实际 `session.config`，最后 `session.ready`。报告规范化后已提交的值，不只是回显请求。宿主工具通过 `config.mcpServers` 提供，没有第二套工具回调 API。

## 完成第一次提示 {#complete-the-first-prompt}

所有提示（包括消息与结构化命令）都使用 `session.prompt`。每个 `prompt.clientMessageId` 恰好对应一个 `session.prompt_result`：

1. 发送标准 `user_message` 时间线快照并带同一 `clientMessageId`，便于宿主合并乐观消息。
2. 新轮次发送 `result: {"type":"turn","turnId":"..."}`；无轮次操作按协议返回相应完成/失败结果。
3. 发送 `state: "started"` 的 `session.turn`。
4. 实际输出变化时发送稳定 item ID 的完整 `timeline.item` 快照。
5. 恰好发送一个终态 `session.turn`：`completed`、`failed` 或 `canceled`。

`send()` 成功只确认接受，不能在实际 Agent 完成前报告完成，也不能吞掉失败。区分请求、客户端消息、会话和轮次 ID。

## 权限与取消 {#permissions-and-cancellation}

发送 `session.permission`，包含会话 ID，以及稳定权限 ID、描述性名称/详情、类型和实际允许操作。例如文件检查器对具体相对文件提供 `allow`/`deny`。处理对应 `session.permission` 响应时，先校验权限 ID 与行为，再执行读取；发送 `session.permission_resolved` 和轮次结果。拒绝的操作不能运行。

权限是结构化操作，不一定是 shell 字符串。保留底层 Agent 的问题/批准选项，不伪造批准或默认选择更宽松权限。取消会话必须停止工作并解决待处理权限。批准等待期间关闭/重新加载插件，不能留下脱离管理的操作继续运行。

## 添加会话专属 Composer 控件 {#add-session-specific-composer-controls}

只有真实 Agent 支持时才声明配置能力，并通过 `session.config` 提供标准 toggle/select 描述符。用户变更通过 `session.configure` 到达；应用后先发送已提交配置，再发 `request.completed`。未应用不能确认成功，控件可随模型变化。

`providerOptions` 是创建会话时传入的不透明 JSON，不会自动生成设置面板。供应商 token 预算应放在实际选项或受支持配置描述符中，不要虚构提示字段。标准配置数据不是可执行客户端扩展。

## 添加持久化与重放 {#add-persistence-and-replay}

仅当底层会话确实可恢复时声明 `session.persistence`。没有 `session.open.persistence` 时新建；存在时恢复对应会话。通过 `session.opened` 或 `session.persistence` 返回不透明持久化值，Paseo 只存储、不解释。

`history: "replay"` 时在 `session.ready` 前发送历史完整时间线；`history: "skip"` 不重放。刷新会关闭并以当前配置/持久化重新打开，所以 open 时重读凭据、环境和 MCP 服务器。普通可恢复会话使用 `restoration: "core"`；Provider 拥有的子会话使用 `restoration: "parent"` 和 `parentSessionId`，恢复父会话时重建子会话。不能把临时内存映射宣称为持久化。

## 添加命令和转向 {#add-commands-and-steering}

消息输入包含结构化文本/图片/附件；命令输入包含名称和参数。只声明真实支持的内容/能力。`session.commands` 发布实际命令。`delivery: "steer"` 请求向活动轮次加输入；不支持时不要声明 `prompt.steer`，并拒绝不支持的输入，不能假装完成转向。`delivery: "auto"` 可新建轮次或无轮次完成命令；两种路径都要保留提示结果关联。

## 发布时间线条目 {#publish-timeline-items}

使用完整 `timeline.item` 快照，对流式文本、工具和待办更新复用 ID。标准类型包括用户/助手消息、推理、工具、待办、错误、通知和压缩。原生客户端渲染数据，不加载插件代码。旧自定义插件载荷和 React 渲染器不是展示 API；重要输出必须能通过标准条目读懂。

```json
{"type":"timeline.item","sessionId":"session-1","item":{"type":"assistant_message","id":"inspection-1","text":"README.md: 42 lines"}}
```

这只是 wire 形状示例，不表示真的检查过文件。

## 适配 ACP Agent {#adapt-an-acp-agent}

使用 [`paseo_plugin::acp`](../../packages/plugin/src/acp/mod.rs)，它是旧 `runAcpProvider` 功能的原生替代。适配器已实现 `Provider`/`Connection`，在 `plugin.run().await` 前注册即可。已有第一方 ACP 集成也保持原生实现；插件不需要导入 daemon 私有模块，更不能保留 JavaScript worker。

```rust
use paseo_plugin::{Plugin, Result, acp::{AcpOptions, AcpProvider}};

fn register_acp(plugin: &mut Plugin, command: Vec<String>) -> Result<()> {
    let provider = AcpProvider::command("my-acp", "My ACP agent", command)?
        .options(AcpOptions {
            wait_for_initial_commands: true,
            ..AcpOptions::default()
        });
    plugin.provider(provider)?;
    Ok(())
}
```

`command` 必须传入真实已安装 Agent 的可执行文件和独立参数；此函数不安装软件，也不自带 Agent。插件自身 manifest 仍指向原生二进制。`--validate` 时也要注册确定性元数据；Agent 探测/连接发生在 Provider 连接阶段，不是注册阶段。

命令适配器遵循宿主 launch 描述符提供的可执行文件、argv 和完整环境；否则使用注册命令与继承环境。会话 `config.env` 覆盖该环境中的对应值。stdout 专用于 ACP JSON-RPC，stderr 被持续读取到插件日志。在 Unix 上，关闭时向拥有的直接子进程 PID 发送 `SIGTERM`，最多等待一秒；必要时发送 `SIGKILL` 并回收子进程，这不是进程组终止。在 Windows 上，关闭时调用 `taskkill.exe /PID <拥有的 PID> /T /F` 终止子进程及其进程树，辅助命令超时为五秒，随后回收直接子进程。辅助命令失败会记录日志，并退回强制终止/回收直接子进程；此时不保证后代进程清理成功。

自定义字节传输使用 `AcpProvider::connector(id, label, factory)?`：异步工厂接收取消 token，返回全新、自有的 `AcpStream`。使用 `AcpStream::new(reader, writer)` 构造，两个端点分别实现 Tokio `AsyncRead`/`AsyncWrite`，且满足 `Unpin + Send + 'static`。流公开 `cancellation` token 和可选的 `task: Option<tokio::task::JoinHandle<()>>`；仅在流拥有连接任务生命周期时挂接任务，并让任务响应取消。关闭时取消流并等待自有任务，采用有界关闭等待，超时后中止任务。不要阻塞 Tokio 运行时线程，也不要假定中止任务会清理独立启动的进程。

能力探测、目录发现和会话分别调用工厂，不要共享活动会话流或返回借用资源。自定义连接器负责其传输特有的进程/网络清理；这部分不会由命令适配器代劳。

适配器协商 Provider 协议 1，支持文本/命令提示、配置和权限。仅当 ACP Agent 声明相应能力且宿主提供时，才启用图片输入、会话列表和持久化。它拒绝转向及结构化输出 schema，不假装支持。`AcpOptions::default()` 使用 10 秒启动超时、全局目录发现（`workspace_only_catalog: false`），默认不等待初始命令。启用 `wait_for_initial_commands` 后，最多等待 `initial_commands_timeout`（默认 1 秒）。启动超时本身不限制提示运行时长。

中断会发送 ACP `session/cancel`，并将待处理权限请求解析为已取消；Agent 必须结束活动提示。默认 2 秒的 `cancellation_timeout` 限制的是新提示替换活动提示时的等待，不是每次单独中断。若前一轮未结束，适配器会关闭传输，而不是启动重叠工作。

ACP 客户端声明 `clientCapabilities: {}`，只处理 Agent 发来的 `session/request_permission`，不提供客户端文件系统或终端服务。权限只覆盖 Agent 报告的请求，不覆盖所有操作系统操作：Agent 与插件仍是拥有 daemon 用户访问权限的受信任原生进程。

### 供应商转换器 {#vendor-transformers}

使用 `.transformer(implementation)` 按顺序添加扩展，并用 `#[paseo_plugin::async_trait]` 实现 `AcpTransformer`：

- `discover(catalog, AcpContext) -> Result<Value>` 转换发现的目录。
- `configure(&ConfigChange, AcpContext) -> Result<bool>` 处理模型/模式/思考或 `{id, value}` 设置变更。仅在实际应用后返回 `true`；`false` 交给其他转换器或适配器正常映射。
- `notification(method, params, session_id) -> Result<Vec<VendorUpdate>>` 处理供应商通知，更新可携带命令、配置、时间线条目或通知。
- `tool_call(snapshot, session_id) -> Result<Value>` 转换完整工具快照（`id`、可选 `name`、`title`、可选 `kind`、`status`、`input`、`output`、`locations`）。
- `AcpContext` 提供适配器的 `session_id` 与 `config`；`ConfigAccess::read().await` 读取当前选项，`set(id, value).await` 应用选项。

通知/工具转换是同步的，确保 wire 更新先于后续 JSON-RPC 响应；异步配置应使用异步 hook。校验供应商载荷，对无关/格式错误数据保持原样。保留普通工具/编辑语义，不要用自定义渲染器替换整个事件流。历史 `provider-acp-transformer` 目录不是可安装原生插件，旧 JavaScript API 必须显式移植到这些 hook。

## 测试并分发 {#test-and-publish}

在隔离测试 daemon 上使用真实外部 Agent 或本地文件示例，覆盖注册/目录、会话创建、真实提示、权限/拒绝、取消、可选转向/持久化/重放，以及活动期间的清理。在声称支持的原生客户端/平台确认标准数据展示。编译、校验元数据和 `running` 状态本身不能证明这些行为。

Git、Cargo 和校验和固定发行来源见[分发指南](publishing.zh-CN.md)。本文不表示已执行测试、安装或发布。明确记录尚未实现的可选能力，不要提前声明。
