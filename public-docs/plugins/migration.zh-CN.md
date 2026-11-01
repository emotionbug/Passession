---
title: 将插件迁移到原生后端运行时
description: 把后端行为移植为 Rust 可执行文件，不保留 JavaScript 或 UI 兼容回退。
nav: 迁移
---

# 将插件迁移到原生后端运行时 {#migrate-a-plugin-to-the-native-backend-runtime}

本文是 **0.13.1 源码迁移说明**，不是发布公告。旧 JavaScript/TypeScript 插件**不能在新运行时执行**。daemon 会拒绝旧运行时清单并给出原生迁移错误，同时保留安装记录和设置；不会启动 Node worker、安装解释器或静默删除/启用旧插件。`PASEO_PLUGIN_NODE`、`PASEO_PLUGIN_WORKER` 不是迁移机制。

迁移需要真实移植，不是重命名入口或修改版本范围。已有 Rust 内置功能保持原生；Kiro 保持禁用，不属于本指南操作。外部 Agent CLI 是独立产品，不是 Paseo 插件运行时。

## 1. 对插件分类 {#1-classify-the-plugin}

| 旧功能 | 原生实现 |
| --- | --- |
| RPC/供应商调用 | `Plugin::rpc`，显式 JSON 校验和真实 Rust I/O。 |
| 生命周期事件/变换 | `Plugin::on`/`before`、取消和资源清理。 |
| 设置 schema/迁移 | `SettingsDefinition`、`Plugin::settings`，兼容存储格式和 revision。 |
| 直接 Provider | `Provider`/`Connection`、真实会话事件、权限和清理。 |
| ACP 适配器 | `paseo_plugin::acp::AcpProvider` 接受命令或自有流，供应商扩展移植到 `AcpTransformer`。它替代 `runAcpProvider` 的功能，不兼容 JavaScript 源码 API。 |
| 用量发现/获取 | `UsageSource`、显式输入校验及账号/凭据边界。 |
| 界面、面板、侧边栏/标题组件、附件选择器、主题 | 没有可执行 Flutter 插件对应项，移除旧 UI 注册。 |
| React 时间线变换/渲染器 | 输出标准协议条目，重要输出不能依赖扩展代码。 |
| 仅客户端插件 | 不是支持的原生插件，不能把空后端称作迁移。 |

原生 Markdown/源码查看和配置描述符是产品功能/数据契约，不是任意 UI 扩展插槽。没有浏览器/webview 回退执行旧客户端代码。

## 2. 构建原生可执行文件 {#2-build-a-native-executable}

以 `paseo plugin init <new-empty-directory> --sdk-path <checkout>/packages/plugin` 为起点，随后移植实际行为。项目包括 `Cargo.toml`、`src/main.rs`、`paseo-plugin.json` 和作者文档。使用 Rust 1.93+、edition 2024 和真实 `paseo-plugin` crate，不要依赖未发布注册表版本。

```json
{
  "id": "review-tools",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "target/release/paseo-plugin-server", "args": [] },
  "build": [["cargo", "build", "--release", "--target-dir", "target"]]
}
```

路径应匹配 Cargo 二进制目标，Windows 使用 `.exe`。文件必须留在插件目录内且为真实原生程序，不能是调用旧入口的脚本。命令是 argv 数组，不是 shell 片段。`Plugin::run` 提供 `--validate` 和正常 initialize/ready IPC，见[参考](reference.zh-CN.md#validation-and-readiness)。

把 I/O 和依赖移植到 Rust，再从**新分发插件**移除旧入口、包管理器清单、编译器配置和纯 UI 模块。源码移植不应删除用户旧安装或设置。开发中使用本地路径，或固定可获取的 SDK Git 修订。

## 3. 显式移植 SDK 调用 {#3-port-sdk-calls-explicitly}

| 旧概念 | 原生 API/来源 |
| --- | --- |
| 服务端贡献入口 | 创建 `Plugin`，在 `run().await` 前完成注册。 |
| `handle`/`defineRpc` schema | `Plugin::rpc`；用 Serde/领域检查校验 `Value` 输入/输出，没有自动 Zod 对应。 |
| `on`/`before` | 异步 `(Value, Context) -> Result<Value>`；before 输入为 `{request: ...}`，null 保留请求。 |
| 返回清理回调 | `Plugin::cleanup`，观察 `Context::cancellation`。 |
| `defineSettings`/`registerSettings` | `SettingsDefinition::new(...).migrate(...)`、`Plugin::settings`。 |
| 服务端 Paseo 客户端 | `context.daemon.client().await?` 返回 `Arc<paseo_client::DaemonClient>`。 |
| Provider 注册 | `paseo_plugin::provider::{Provider, Connection, Events, Metadata}`。 |
| 用量注册 | `UsageSource::new`、`Plugin::usage_source`、`paseo_plugin::usage` 类型/函数。 |
| JavaScript 进程辅助函数 | 真实 Rust 进程所有权和平台清理，不是兼容包装。 |

作者 SDK：[packages/plugin](../../packages/plugin/src/lib.rs)；宿主：[daemon 插件](../../packages/daemon/src/plugins.rs)；客户端：[Rust SDK 参考](../sdk/reference.zh-CN.md)。旧 SDK 导出不能证明支持 UI。

## 4. 保留数据与行为 {#4-preserve-data-and-behavior}

需要兼容时保持安装 ID、RPC 名、载荷含义和设置 ID。设置保留主机本地 `{version, values}` 格式、不透明 revision、默认值、迁移校验、冲突检测和无效数据保留。对真实旧数据测试迁移，解析失败不能覆盖数据。设置 API 不生成原生设置界面。

前置钩子返回**完整变更后请求**或 null，保留先前变换与无关字段。`agent.session_open` 仅改变启动环境覆盖，不绕过 daemon 策略。事件载荷、取消与清理和注册成功一样重要。

Provider 保留能力协商、提示回执、权限决定、取消、时间线身份、可选持久化/重放和子会话恢复；未实现能力不能宣传。标准用户/助手文本、推理、工具、待办、错误、通知和压缩无需自定义代码即可展示。见 [Provider 指南](providers.zh-CN.md)。

用量只发现指定范围账号，标识/input/日志不能包含秘密，不刷新或改写其他 CLI 凭据。普通宿主操作使用 daemon 客户端桥接，不再自行重复实现 RPC。桥接是可信 daemon 访问，不是安全沙箱。

## 5. 更换获取和构建要求 {#5-change-acquisition-and-build-requirements}

使用目录、Git、Cargo 或校验和固定的 HTTPS 发行来源。旧 npm 来源不能通过 npm 执行/更新。**不能用移除/重装来保留设置**：移除会删除该安装的设置目录，重复 ID 安装也会被拒绝。目前没有原子化同 ID 托管来源替换命令。

明确执行无损的**目录运行时迁移**：

1. 用 `paseo plugin disable <id>` 禁用旧安装。
2. 另行获取/构建可信原生移植，并执行 `--validate <directory> <daemon-version>`。
3. 备份选定 daemon 配置/设置。只把既有 `plugins[<id>]` 配置条目改为 `{"source":"directory","path":"/absolute/path/to/native-plugin","enabled":false}`，保留安装 ID 和全部其他配置条目。
4. 用 `paseo reload --json` 应用配置，再明确启用/重新加载同 ID，并查看状态/日志。

这样保留 `plugin-settings/<id>` 和旧托管收据/文件，但**不迁移旧托管来源记录**；来源管理命令可能继续报告旧来源迁移错误。托管收据/来源替换流程是单独缺少的能力，不能通过删除记录来掩盖。先在测试副本上操作，真实数据迁移前保留备份。

Git/Cargo 源码插件需要主机 Rust 工具链和声明构建依赖；预编译插件只需兼容运行依赖。仓库 daemon 用 `cargo xtask build-daemon` 构建，无需 Node 构建步骤。使用独立测试 daemon/home，不操作用户生产 daemon。远程安装路径必须在 daemon 主机存在，CLI 不上传文件。

分发清单、真实原生程序或可复现源码/构建输入、资源和声明。固定依赖，`--locked` 构建提交 Cargo 锁，说明 OS/架构。见[分发指南](publishing.zh-CN.md)。放宽 `requirements.paseo` 不能把旧插件变成原生插件。

## 6. 实际覆盖迁移后的行为 {#6-exercise-the-migrated-behavior}

对于脚手架布局，在隔离测试主机上：

```sh
cargo build --release --target-dir target
./target/release/paseo-plugin-server --validate . 0.13.1
paseo plugin install /absolute/path/to/native-plugin
paseo plugin ls <installed-id>
paseo plugin logs <installed-id>
```

使用实际可执行文件名，Windows 加 `.exe`。修改后重新构建并执行 `paseo plugin reload <installed-id>`。根 `pluginsEnabled` 和安装启用状态都应允许执行。覆盖每个保留 RPC、钩子、设置/迁移、用量和真实 Provider 轮次/权限路径，包括活动时清理。`running` 和校验通过不代表行为覆盖。本文不声称已发布、部署或执行测试。

仓库维护的可运行示例是 [workspace-inspector](index.zh-CN.md#run-the-repository-example)，不是旧 JavaScript 示例目录。纯客户端示例不能靠无操作后端变得有用，应明确说明不支持的功能。
