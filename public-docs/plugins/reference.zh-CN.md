---
title: 插件参考
description: 原生可执行文件清单、Rust SDK 契约、IPC 和来源管理。
nav: 参考
---

# 插件参考 {#plugin-reference}

先阅读[快速上手](index.zh-CN.md)和[迁移指南](migration.zh-CN.md)。本文描述 **0.13.1 源码检出**，不表示已经发布或部署。

## 运行时边界 {#runtime-boundary}

daemon 和内置功能由 Rust 实现。第三方插件是由 daemon 启动的原生可执行文件，通过有界 JSONL stdin/stdout 通信；没有动态库 ABI、JavaScript worker 或解释器回退。Flutter 渲染受支持的协议数据，不执行插件 UI。插件、校验和构建命令都是无沙箱的 daemon 用户代码。

权威实现：[SDK](../../packages/plugin/src/lib.rs)、[设置](../../packages/plugin/src/settings.rs)、[Provider trait](../../packages/plugin/src/provider.rs)、[daemon 桥接](../../packages/plugin/src/daemon.rs)、[宿主清单](../../packages/daemon/src/plugins/manifest.rs)和 [CLI](../../packages/daemon/src/cli/catalog/plugins.rs)。SDK 未发布时使用本地路径或明确固定修订的 Git 依赖。

## 项目文件 {#project-files}

`paseo plugin init <directory> --sdk-path <checkout>/packages/plugin` 在空目录中生成 Rust 可执行项目、清单和作者文档，不自动构建。该选项写入绝对 SDK 路径依赖；未指定时使用 Git 依赖。激活前必须编译可执行文件。

### 要求和运行时入口 {#runtime-entries}

```json
{
  "id": "review-tools",
  "name": "Review tools",
  "description": "Reviews local changes",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "target/release/paseo-plugin-server", "args": [] },
  "build": [["cargo", "build", "--release", "--target-dir", "target"]]
}
```

Windows 使用 `.exe`。插件 ID 以小写字母开头，只含小写字母、数字或连字符。`server.args` 必须存在，可以为空；参数直接传递，不拼接 shell 字符串。可执行文件必须是规范化相对路径，解析符号链接后仍在插件目录内，并为常规 ELF/Mach-O/PE 原生文件。绝对路径、路径穿越、脚本和解释器包装器被拒绝；Unix 文件必须有执行权限。有效文件头不保证架构与主机匹配。

只声明实际支持的 Paseo semver 范围。省略 `requirements.paseo` 意味着历史默认值 `<0.8.0`，不是任意版本兼容。空/无效范围和未知要求键会失败。仅改变版本范围不能移植插件。

### 校验和就绪 {#validation-and-readiness}

宿主在 `server.args` 后追加 `--validate <插件绝对目录> <daemon版本>`。程序必须成功退出，并打印不超过 1 MiB 的单个 JSON 对象：

```json
{"valid":true,"plugin":{"id":"review-tools"},"methods":[],"hooks":{"events":[],"before":[]},"providers":[],"usageSources":[]}
```

`Plugin::run()` 实现此模式，读取清单并返回已注册元数据，不调用处理函数。但程序启动和注册代码仍会执行，所以校验**不是安全检查不可信代码的方式**。

正常启动收到 `initialize`，包含 `pluginId`、`pluginDirectory`、`appVersion`，使用设置时还包含 `settingsDirectory`。程序返回相同的注册字段，以 `type: "ready"` 取代 `valid`/`plugin`。应在 `run()` 前完成所有注册。RPC/钩子/用量请求返回匹配 `requestId` 的结果或错误；Provider 连接另有接受通知和事件。帧上限为 16 MiB，输出有背压。stdout 仅用于 IPC；诊断使用 `Context::log(...).await` 或 stderr。

## 入口和清理 {#entry-point-and-cleanup}

以下完整 `src/main.rs` 使用脚手架依赖：

```rust
use paseo_plugin::{Plugin, Result, ensure};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<()> {
    let mut plugin = Plugin::new();
    plugin.rpc("greeting.create", |input, _context| async move {
        let name = input["name"].as_str().unwrap_or("");
        ensure!(!name.is_empty(), "name must be a nonempty string");
        Ok(json!({"message": format!("Hello, {name}!")}))
    })?;
    plugin.on("agent.turn_ended", |event, context| async move {
        context.log(format!("Turn ended: {} ({})", event["agent"]["id"], event["outcome"])).await?;
        Ok(Value::Null)
    })?;
    plugin.run().await
}
```

回调接收 `serde_json::Value` 和 `Context`，异步返回 `Result<Value>`。**输入和输出需要作者自行校验**，可使用 Serde、领域检查或显式 schema 校验器。`Plugin::rpc` 校验注册名称和重复项，不自动校验应用载荷。注册不会生成按钮或界面；消费者通过 daemon 协议调用插件 RPC。

`Plugin::cleanup` 用于释放自己拥有的资源，回调接收 null 和 context，等待任务结束并关闭套接字/进程/订阅后返回 `Ok(Value::Null)`。`Context::cancellation` 表示请求取消或关闭。重新加载、禁用、移除和 daemon 退出会结束进程与注册。清理有截止时间；不要依赖崩溃或强制终止后仍会调用清理。没有客户端连接时钩子也会运行。

## 生命周期钩子 {#lifecycle-hooks}

| 注册 | 输入和结果 |
| --- | --- |
| `plugin.on(name, callback)` | 接收事件 JSON，处理后返回 null；按注册顺序执行，错误会记日志但不跳过后续处理函数。 |
| `plugin.before(name, callback)` | 接收 `{"request": ...}`；返回 null 保留请求，或返回完整的变更后请求。 |

事件包括 `agent.created`、`agent.closed`、`agent.turn_started`、`agent.turn_ended`、`agent.permission_requested`、`agent.permission_resolved`、`agent.archived`、`workspace.created`、`workspace.archived`。公开后台 Agent 参与，私有临时辅助 Agent 不参与。归档通知不证明清理已完成。轮次结束包含 `agent`、`turnId`、`outcome`（`completed`、`failed`、`canceled`）；不要依赖已废弃且为空的 `timeline`。

前置钩子：`agent.create` 可变更除 `cwd` 外的公开配置以及启动 `env`；`agent.session_open` 仅可变更 `env`；`workspace.create` 变更显式工作区创建请求。保留无关字段与前序变更。`env` 是启动覆盖值，不是完整继承环境。会话打开包括创建、恢复、刷新、导入和历史读取。遵守取消；不要把每个权限请求都当成 shell 命令，也不要把权限解决当成轮次结束。

## 已连接主机和 Paseo SDK {#connected-hosts}

`context.daemon.client().await?` 返回 `Arc<paseo_client::DaemonClient>`。惰性连接通过插件 IPC 桥接文本和二进制帧，不需要额外令牌或网络连接。宿主关闭使缓存会话失效，下次调用会重新打开逻辑会话。普通 Agent、工作区、权限、Provider 和配置操作使用 [Rust 客户端 SDK](../sdk/reference.zh-CN.md)。需要历史时读取有界片段，不要默认记录私密对话。主机 ID、路径和工作区 ID 属于各 daemon 连接，不能跨主机混用。

## 持久化设置 {#settings-screens}

通过 `plugin.settings(...)` 注册 `SettingsDefinition::new(id, positive_version, validator)`。校验器接收 JSON 并返回包含默认值的规范化 JSON；`{}` 必须得到默认文档。可选 `.migrate(|values, from_version| async move { ... })` 在校验前转换旧数据。完整例子见[文件检查设置](../../plugin-examples/workspace-inspector/src/lib.rs)。

返回的可克隆 `Settings` 提供 `read().await`、`write(revision, values).await`、`reset(revision).await`、`subscribe()`（Tokio broadcast receiver）。ID 符合 `[a-z][a-z0-9_-]*`。存储以主机为范围，保留 `{version, values}` 格式；schema 版本与不透明写入 revision 不同。

- 读取返回 `status: "ready"`、`values`、`revision`，或 `status: "invalid"`、`error`、`revision`。无效数据保留至明确修复/重置。
- 写入/重置必须携带读取 revision；过期写入返回 `conflict`，成功返回 `saved`，无效数据返回 `invalid`。失败写入不通知订阅者。
- 订阅返回状态；出现 lag 错误时重新读取。订阅者失败不会回滚已保存写入。
- 自动注册 `settings.<id>.read`、`.write`、`.reset` RPC，变更时发出 `settings.changed`。

这些是数据 API，不是可执行设置界面。

## Provider {#providers}

实现 `paseo_plugin::provider::{Provider, Connection}` 并调用 `plugin.provider(...)`。`Provider::connect` 接收请求 JSON、`Events` 和 `Context`，返回 `Arc<dyn Connection>`。仅协商宿主提供的版本/能力。`Connection::send` 表示接受输入，不表示轮次完成。使用 `Events::emit` 发送符合协议的 JSON；`close` 必须停止任务/进程并解决待处理权限。可选状态/目录缓存能力必须与元数据一致。完整会话、权限、配置、持久化和时间线契约见 [Provider 指南](providers.zh-CN.md)。

`Metadata::icon_path` 是插件目录内的 SVG 相对文件路径，不是内联 SVG 或 URL。`UsageSource::icon` 则是**内联 SVG 文本**，二者不同。宿主把 SVG 限制为 64 KiB，拒绝脚本、foreignObject、样式、事件处理属性和外部引用等主动内容。展示页 PNG 另见[分发契约](publishing.zh-CN.md#icons-and-screenshots)。

ACP Agent 应注册 [`paseo_plugin::acp::AcpProvider`](../../packages/plugin/src/acp/mod.rs)，不必自行重建 Provider 协议。`AcpProvider::command(id, label, argv)` 与 `::connector(id, label, factory)` 返回 `Result<AcpProvider>`；`.options(AcpOptions)`、`.transformer(impl AcpTransformer)`、`.description(...)`、`.icon_path(...)` builder 返回 Provider。连接器异步返回全新、自有的 `AcpStream`，并接收取消 token。`AcpTransformer` 提供有序目录发现/配置，以及通知/工具快照变换；`AcpContext::config` 提供 `ConfigAccess::read`/`set`。生命周期和能力边界见 [ACP 指南](providers.zh-CN.md#adapt-an-acp-agent)。这是旧 ACP 辅助功能的原生实现，不是 JavaScript 源码兼容层。

## 用量来源 {#usage-sources}

`UsageSource::new(id, label, discover, fetch)` 通过 `plugin.usage_source(...)` 注册。两个异步回调都接收 JSON 和 `Context`；访问文件或服务前自行校验 scope/input，没有自动 Zod/schema 解析器。

公开 [`paseo_plugin::usage` 模块](../../packages/plugin/src/usage.rs) 提供 Serde 类型 `Scope`、`Account`、`Report`、`Problem`、`Window`、`Balance`、`Detail`、`Unit`、`Tone`。用 `serde_json::from_value` 解析输入，`serde_json::to_value` 序列化类型化输出。辅助函数包括 `hash_account_key`、`used_pct_of`、`tone_from_used_pct`、`balance_tone_from_remaining`、`window_from_used_pct`、`window_from_reported_duration`。时长函数依次接收报告秒数、可选稳定范围 ID/标签、明确的未知 ID/标签/短标签、已用百分比和重置时间；不能把重置倒计时当成周期时长。

- 发现接收全局或会话范围，返回稳定 `key`、可选 `label`/`harness` 和标识凭据存储的 JSON `input`，**不能包含凭据**。会话范围含 `provider`、可选 `model` 和启动 `env`；只返回此会话账号，无关或未登录时返回 `[]`，不回退扫描其他 Agent 的存储。
- 获取返回带窗口及可选余额/详情的 `available`、带类型化问题的 `unavailable`，或带消息的 `error`。重新读取凭据，不兑换刷新令牌，不改写其他 CLI 的存储。
- 账号键使用 `[A-Za-z0-9._-]` 中的 1–128 个字符。敏感身份应哈希处理；身份应跨令牌轮换稳定，不能直接使用邮箱或令牌。
- 不可用原因区分 `expired`（`expiresAt`、可选 `refreshedBy`）、`rejected`（`status`、可选 `refreshedBy`）与 `no_quota`（`detail`）。`refreshedBy` 是 CLI 名，不是给用户的指令。
- 窗口名来自报告时长或供应商名称，不是返回顺序或重置倒计时；ID 在重置/重排后保持稳定。

内置用量集成已经由 Rust 执行，不需要额外可执行文件或自定义原生面板。

## 插件来源 {#plugin-sources}

| 来源 | 选择器 |
| --- | --- |
| 主机目录 | `/srv/paseo/plugins/review` 或 `./review` |
| GitHub/Git | `github:acme/paseo-review`、`git:acme/paseo-review`、`git:https://git.example/acme/review.git` |
| Cargo crate | `cargo:paseo-review@1.2.0`（精确 semver），或 `cargo:paseo-review`（最新稳定且未撤回版本） |
| 发行归档 | `release:https://releases.example/review-1.2.0.zip` 加 `--version 1.2.0 --sha256 <digest> --format zip` |
| 注册表 | `owner/slug` 或 `registry.example/owner/slug` |

表中名称/URL 仅演示语法，不代表已有发布插件。裸 `owner/slug` 是注册表 ID，不是本地路径或 GitHub 简写。本地相对路径加 `./` 或 `../`。使用 `--host` 时提供 daemon 上的绝对路径，不上传文件。`--path` 选择来源内的子目录；Git 也接受 `github:acme/monorepo:plugins/review`。`--ref` 选择 Git 分支/标签/提交。安装 Cargo 版本使用 `@exact-semver`，不是 `--version`。

发行 URL 要求无内嵌凭据的 HTTPS、明确版本和 64 位小写十六进制 SHA-256。`--format` 为 `tar.gz` 或 `zip`，可从已识别的 URL 后缀推断。下载有大小限制，校验和通过后才安全解包。来源获取不是沙箱：后续准备与校验仍执行受信任代码。

默认注册表为 `https://plugins.paseo.sh`，`PASEO_PLUGIN_REGISTRY` 可替换默认值。私有授权配置为 `pluginRegistries: {"host":{"authorization":"Bearer token"}}`，凭据不转发到产物主机或重定向。已有安装保留来源/注册表身份。注册表必须提供原生 Git/Cargo/发行记录；本检出不能证明公共注册表已经迁移或发布原生插件。npm 来源和旧 JavaScript 记录不会执行，参见[迁移](migration.zh-CN.md)。

`--id` 覆盖安装身份，重复 ID 会失败。移除目录安装保留用户源码；移除托管安装只删除其托管文件，不删除任意外部目录。

## 准备命令 {#preparation-commands}

清单 `build` 是非空 argv 数组的数组（见上例），在暂存插件目录直接执行，不隐式使用 shell。命令可下载依赖或运行 Cargo 构建脚本，权限为 daemon 用户。安装/更新先准备并校验候选，再激活；失败保留此前托管安装。不从锁文件推断命令。可复现分发应提交 `Cargo.lock` 并使用 `cargo build --locked --release --target-dir target`；初始脚手架命令有意不要求尚未生成的锁文件。本地目录重新加载前应先构建。

## CLI 参考 {#cli-reference}

```sh
paseo plugin init /absolute/path/to/plugin --sdk-path /absolute/path/to/paseo/packages/plugin
paseo plugin install /absolute/path/to/plugin
paseo plugin ls review
paseo plugin logs review
paseo plugin update review --check
paseo plugin update review --yes
paseo plugin update --all --check
paseo plugin update --all --yes
paseo plugin update review --version 1.2.0
paseo plugin update review --ref v2
paseo plugin reload review
paseo plugin disable review
paseo plugin enable review
paseo plugin remove review
```

把 `review` 换成实际安装 ID。`install` 也可写作 `add`。普通更新先预览并要求 `--yes`；`--check` 永不应用。显式 `--version` 选择 Cargo 版本，`--ref` 选择 Git；显式选择授权单个目标，不能配合 `--all`。发行更新要求 `--url <HTTPS URL> --version <version> --sha256 <digest> [--format zip|tar.gz]`，且 URL origin 与已安装来源一致，没有隐式最新发行发现。

直接 Git 更新跟随默认 HEAD，Cargo 选择稳定未撤回版本，注册表跟随审查固定版本。目录安装跳过。提案绑定不可变产物身份/校验和，发生变化就失败，不会静默替换内容。批量结果彼此独立。`ls` 只读取已安装元数据，不查询远端。

## 调试后端输出 {#debug-backend-output}

`paseo plugin logs <id>` 返回近期快照，不是实时跟随。使用 stderr 或 `Context::log`；任意 stdout 文本会破坏 IPC。不要记录秘密。二进制缺失、架构不符、无执行权限、版本不兼容、元数据错误或运行失败都是实际加载错误：修复原因，必要时重新构建再加载。旧运行时需要原生移植，不是配置解释器。`running` 不证明 Provider 正确或 UI 兼容。
