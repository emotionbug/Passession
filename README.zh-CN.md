# Paseo Sessions

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

Paseo Sessions 是运行和审阅编程智能体工作的原生环境。原生 Rust（Makepad）客户端面向 **Linux、macOS、Windows、Android 和 iOS**；守护进程与 `paseo` CLI 同样使用 Rust。桌面客户端可以管理本地守护进程，移动客户端仅连接其他机器上的守护进程。

当前检出版本为 **0.13.6**。请使用[本仓库的发布产物](https://github.com/emotionbug/Passession/releases)；上游下载不是本仓库的构建。自行构建或分发时，请阅读[发布要求](docs/release.md)。

## 跨主机工作与功能范围

侧栏按 **主机 → 目录 → 会话**组织。即使仅配置一台主机，也有独立的主机行。空目录会保留，会话按最近活动排序；主机和目录可独立折叠，目录折叠状态会持久保存。不同主机上的相同路径不会合并。

搜索匹配主机、目录名称和路径、会话标题及提供商，并展开匹配项的祖先节点，不覆盖已保存的折叠偏好。目录旁的加号在对应主机和检出目录内创建会话。“添加项目”使用主机端文件夹浏览器，远程路径不会通过本机系统文件选择器选择。工作区仍用于编排、worktree、脚本和 Command Center 导航，但不再是侧栏分组控件。关闭聊天标签不会归档会话；现有会话 ID、历史、文件、配置与存储保留。

- 智能体聊天、权限、提供商/模型选择、配置档、会话导入和历史。
- Markdown 渲染、文件浏览与源码编辑；Git diff 先显示摘要，按需加载补丁正文。
- 交互式终端：在桌面或移动端都可以在任意主机上启动**终端**会话。所有 shell、初始化脚本和服务都运行在守护进程拥有的 PTY 中；脚本以**只读日志**显示。两端的终端仿真都使用 Ghostty 的 VT 引擎。
- 定时任务、MCP 工具与服务器、已连接主机和远程目录选择。
- 原生桌面集成与本地守护进程生命周期管理；移动端仅远程连接。
- 保留一般音效播放，不提供语音识别、语音对话或 TTS。

不提供产品 Web 客户端、Electron、Chromium、嵌入式浏览器、浏览器自动化或可执行 React UI 插件兼容性。外部链接在应用外打开；未使用的网站已删除，文档仍保留在仓库中。**Paseo 运行时、SDK 与构建工具不需要 Node 或 TypeScript。** 智能体 CLI 是外部程序，可能有自己的运行时要求。

内置提供商与用量集成使用 Rust。后端插件使用原生可执行文件和 Rust `paseo-plugin` SDK；已有 JavaScript/TypeScript 插件需要迁移到新协议。参见[插件文档](docs/plugins.md)。

## 从源码构建和运行

安装 Rust/Cargo、Zig 0.15.2（用于构建 Ghostty 终端库）和目标平台原生工具链。至少安装并配置一个智能体 CLI，例如 [Claude Code](https://docs.anthropic.com/en/docs/claude-code)、[Codex](https://github.com/openai/codex)、[Copilot](https://github.com/features/copilot/cli/)、[OpenCode](https://github.com/anomalyco/opencode) 或 [Pi](https://pi.dev)。完整前置条件见[开发指南](docs/development.md)。

```sh
cargo fetch --locked
cargo xtask build-daemon
cargo xtask dev app
```

也可分别启动开发守护进程和原生客户端：

```sh
cargo xtask dev daemon
# 在另一个终端中：
cargo xtask dev app
```

开发环境使用本检出的 `.dev/paseo-home` 和端口 `6768`，与正常运行的 `~/.paseo`、端口 `6767` 隔离。构建和发布命令通过 Rust `paseo-xtask` crate 执行。

```sh
cargo xtask build-gui linux
cargo xtask build-gui macos
cargo xtask build-gui windows
cargo xtask build-gui android
cargo xtask build-gui android --arch arm64
cargo xtask build-gui ios
```

桌面构建须在对应操作系统上执行，iOS 需要 macOS/Xcode。产物和构建描述写入 `artifacts/native/`。Android/iOS 不含守护进程；Android 名称为 **Paseo Sessions**，包名 `sh.paseo.sessions`。调试签名 APK 或未签名 iOS 应用归档不等于可上架版本，参见 [Android](docs/android.md) 与[发布指南](docs/release.md)。

无头主机可使用生成的 `target/debug/paseo`（Windows 为 `paseo.exe`）；设置 `CARGO_TARGET_DIR` 时使用对应目录。必须保留相邻生成的运行时资源：

```sh
./target/debug/paseo onboard
./target/debug/paseo daemon start
./target/debug/paseo daemon status
```

`cargo xtask build-daemon` 生成调试构建及资源；`cargo xtask build-daemon --release` 生成 `target/release/` 下的优化**本地构建**及资源，不代表经过发布验证的产物。`paseo onboard` 执行显式设置；直接运行 `paseo` 打开 CLI 聊天，不执行安装或引导。

正常数据目录为 `~/.paseo`，可通过 `PASEO_HOME` 或 `--home` 覆盖。`config.json`、智能体历史、项目/工作区元数据与连接凭据沿用原有持久化约定。用 `--host` 连接远程守护进程，需要时设置 `PASEO_PASSWORD`。SSH 传输连接已存在的守护进程，不会代为安装。[Docker 指南](docs/docker.md)构建的是原生守护进程容器，不在 HTTP 根路径提供应用。

## CLI 与 SDK

在主机设置中的智能体配置档上选择 **Use for CLI**，可设置默认提供商、模型、模式和思考级别。显式 CLI 选项优先；冲突的提供商/模型参数会在连接前报错。

```sh
paseo
paseo --resume <session-id>
paseo run --provider codex "review this checkout"
paseo ls
paseo attach <session-id>
paseo send <session-id> "also add tests"
paseo import --provider pi <native-session-id>
paseo run --host workstation.local:6767 --cwd /workspace --provider codex "review the diff"
```

外部集成使用 Rust SDK `paseo-client`，位于 `packages/client`；共享通信类型与中继加密库 `paseo-protocol` 位于 `packages/protocol`。守护进程和原生客户端使用相同的 crate。原有 TypeScript SDK 已由 Rust SDK 取代，而非通过包装层运行。

## 源码与验证范围

| 部分                                                       | 路径                                                    |
| ---------------------------------------------------------- | ------------------------------------------------------- |
| Rust 守护进程、CLI、提供商、存储、supervisor、桌面主机 IPC | `packages/daemon`                                       |
| 五平台原生客户端                                           | `packages/gui`, `packages/gui-core`                     |
| 桌面与移动端系统集成                                       | `packages/gui-platform`                                 |
| 守护进程与客户端的 Ghostty 终端仿真                        | `packages/terminal`                                     |
| 打包资源与安装程序                                         | `packaging`                                             |
| Rust SDK 与共享协议                                        | `packages/client`、`packages/protocol`                  |
| 原生后端插件 SDK                                           | `packages/plugin`                                      |
| Rust 中继与语法高亮                                        | `packages/relay`、`packages/highlight`                  |
| Rust 构建与发布工具                                        | `xtask`                                                |

修改运行时边界前请阅读[架构](docs/architecture.md)、[开发](docs/development.md)、[测试](docs/testing.md)和[发布](docs/release.md)文档。Linux 已有守护进程和桌面主机生命周期的实际执行证据，原生 Rust 客户端本身的执行证据单独记录。此次迁移未在 Windows、macOS、Android 或 iOS 实际运行。跨平台 CI 定义不等于执行证据，完整限制以测试文档为准。

## 许可与分发

Paseo 源码采用 Apache-2.0，依赖保留各自许可证。打包生成 Cargo 依赖与捆绑资源的声明；这些声明不是合规认证或对应源码提供承诺。分发时请保留声明并审查各项依赖的义务。详见[分发要求](docs/release.md#distribution-requirements)。

## 社区与相关项目

[Discord](https://discord.gg/jz8T2uahpH) · [Reddit](https://www.reddit.com/r/PaseoAI/) · [GitHub Sponsors](https://github.com/sponsors/boudra) · [赞助](https://paseo.sh/zh-CN/sponsor)

- [getpaseo/paseo-relay](https://github.com/getpaseo/paseo-relay)：独立的 Elixir 分布式 relay 项目。
- [paseo-vscode](https://marketplace.visualstudio.com/items?itemName=hinnes.paseo-vscode)：VS Code 扩展；其兼容性不代表本次原生迁移已验证。
