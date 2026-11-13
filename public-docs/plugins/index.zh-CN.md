---
title: 插件快速上手
description: 使用 Rust 构建并管理受信任的原生后端扩展。
nav: 快速上手
---

# 插件快速上手 {#plugin-quickstart}

Paseo 插件是在 daemon 主机上运行的**独立原生可执行文件**。Rust `paseo-plugin` SDK 提供有界逐行 JSON IPC、RPC、生命周期钩子、设置、Provider、用量来源及 daemon 客户端桥接。它不是 Rust 动态库，也不在客户端内运行。

本文描述 **0.13.1 源码检出，不代表版本已经发布**。不再有 JavaScript worker、内嵌编译器或 Node 回退。旧 JavaScript 插件必须[显式移植](migration.zh-CN.md)。原生客户端不执行插件界面、React 组件、面板、主题或时间线渲染器；移动端远程连接，不运行插件。

## 准备 daemon 主机 {#prepare-the-daemon-host}

构建源码插件需要 Rust 1.93 或更新版本（edition 2024）；Git 来源还需要 Git。预编译插件需要匹配操作系统、架构和运行库，仅运行时不需要 Rust 编译器。仓库 daemon 使用 `cargo xtask build-daemon` 构建。请使用隔离的开发 daemon，不要拿现有生产安装做实验。

插件及其准备命令都是**受信任、无沙箱的代码**，拥有 daemon 用户的文件、进程、网络和凭据权限。校验也会运行可执行文件；安装前应审查源码、依赖、产物和更新。

## 创建插件 {#create-a-plugin}

所需 SDK 修订真正可从远端获取前，使用本地源码依赖：

```sh
paseo plugin init /absolute/path/to/workspace-plugin --sdk-path /absolute/path/to/paseo/packages/plugin
cd /absolute/path/to/workspace-plugin
cargo build --release --target-dir target
./target/release/paseo-plugin-server --validate . 0.13.1
```

Windows 脚手架使用 `target/release/paseo-plugin-server.exe`，调用 `.\target\release\paseo-plugin-server.exe`。跨系统移动项目时，应修改 `server.executable` 的后缀。

脚手架创建 `Cargo.toml`、`src/main.rs`、`paseo-plugin.json`、`README.md`、`OVERVIEW.md` 和 `.gitignore`。它注册 `greeting.create` RPC：输入 `{"name":"World"}` 返回 `{"message":"Hello, World!"}`。创建时不下载依赖或编译。未指定 `--sdk-path` 时，SDK 依赖指向 Paseo Git 仓库；分发前应固定包含所需 crate 的修订，不能假定已发布到 crates.io。

清单声明 `runtime: "native"`、插件目录内的 `server.executable` 以及 `server.args: []`。脚手架还声明 Cargo 准备命令。完整契约见[参考](reference.zh-CN.md)。

## 安装并试用 {#install-and-try-it}

在选定的**测试 daemon** 上：

```sh
paseo plugin install /absolute/path/to/workspace-plugin
paseo plugin ls workspace-plugin
paseo plugin logs workspace-plugin
```

daemon 根设置 `pluginsEnabled` 省略时默认为 `true`；设为 `false` 可禁用所有外部插件。安装条目的启用状态也必须允许执行。显式的全局或单个安装禁用设置保持不变。修改 `config.json` 的根设置后使用 `paseo reload --json`；`paseo plugin enable workspace-plugin` 仅启用该安装。`running` 只表示激活成功，不证明所有 RPC/Provider 路径正确，也不代表兼容旧插件 UI。

远程管理时，把 `--host <url>` 放在管理命令之前，使用 **daemon 主机上的绝对路径**。CLI 不上传本地文件；`plugin init` 在本地创建文件。

## 运行仓库示例 {#run-the-repository-example}

Linux/macOS 上从仓库根目录执行：

```sh
cargo build -p paseo-workspace-inspector --target-dir plugin-examples/workspace-inspector/target
./plugin-examples/workspace-inspector/target/debug/paseo-workspace-inspector --validate plugin-examples/workspace-inspector 0.13.1
paseo plugin install ./plugin-examples/workspace-inspector
paseo plugin ls workspace-inspector
paseo plugin logs workspace-inspector
```

Windows 上给示例清单的 `server.executable` 加 `.exe`，并以 `.\plugin-examples\workspace-inspector\target\debug\paseo-workspace-inspector.exe` 调用相同的校验参数。

[示例实现](../../plugin-examples/workspace-inspector/src/lib.rs) 是真实的本地文件检查器，不是 AI 服务：

- `file.inspect` 接收 `{"cwd":"/absolute/workspace","path":"README.md"}`，返回有大小限制的 UTF-8 文件统计和预览。直接 RPC 读取由调用者明确请求，不会弹出 Provider 权限请求。
- `lifecycle.counts` 返回当前进程观察到的 `agent.created` 次数。
- `inspection` 设置持久化 `maxBytes`（默认 65536，范围 1–1048576）。会话打开钩子向启动覆盖环境添加 `PASEO_INSPECT_MAX_BYTES`。
- 选择 `workspace-inspector` Provider 和 `local-file` 模型，发送相对文件路径或使用 `inspect` 命令，然后批准或拒绝读取。示例拒绝路径穿越、符号链接、过大文件和非 UTF-8 数据，但不是对抗恶意并发文件系统修改的沙箱。
- 示例不声明持久化、转向、配置变更、图片或输出 schema 能力。

## 编辑并重新加载 {#edit-and-reload}

重新加载前，先构建到清单指定的位置：

```sh
cargo build --release --target-dir target
paseo plugin reload workspace-plugin
paseo plugin ls workspace-plugin
paseo plugin logs workspace-plugin
```

这些命令用于脚手架目录；`workspace-inspector` 应使用前面的 debug 构建命令。重新加载会启动新的可执行文件，不会按需编译 Rust。为任务、子进程、订阅、文件和套接字注册清理逻辑，并遵守取消通知。

## 安装和更新共享插件 {#install-and-update-shared-plugins}

支持目录、Git、Cargo crate、校验和固定的 HTTPS 发行归档，以及兼容的注册表来源。参数见[来源参考](reference.zh-CN.md#plugin-sources)，分发与信任要求见[发布指南](publishing.zh-CN.md)。

```sh
paseo plugin update workspace-plugin --check
paseo plugin update workspace-plugin --yes
```

目录来源应直接编辑并重新加载，不进行远程更新。普通托管更新需要 `--yes`；`--check` 仅预览。阅读本文不会执行安装、发布或部署。
