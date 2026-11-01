---
title: 快速上手
description: 从当前源码构建原生 Paseo 客户端和 Rust daemon。
nav: 快速上手
---

# 快速上手 {#getting-started}

Paseo Sessions 使用 Flutter 构建 **Linux、macOS、Windows、Android 和 iOS** 原生客户端，daemon 与 CLI 使用 **Rust**。桌面或服务器运行 Agent，Android 和 iOS 远程连接。客户端保留 Markdown、源码编辑、摘要优先的 diff、远程文件夹选择和只读脚本日志，侧边栏按 **主机 → 目录 → 会话** 组织。

请使用[本仓库的发布产物](https://github.com/emotionbug/paseo/releases)获取原生构建；上游下载和应用商店版本是独立产品。源码构建和分发要求见仓库 `docs/release.md`。

## 前置条件 {#prerequisites}

在 daemon 所在机器安装并认证至少一个 Provider CLI。Paseo 管理这些程序，但不提供你的账号凭据。Git 以及相关代码托管操作所需的 CLI 也应配置好。

源码构建需要 Rust/Cargo；客户端还需要 Flutter 和目标平台的原生工具链。桌面构建须在相应操作系统上运行；iOS 需要 macOS/Xcode。Paseo 的构建工具、SDK 和原生插件运行时不需要 Node 或 TypeScript，外部智能体 CLI 仍有各自的前置条件。

## 桌面应用 {#desktop-app-recommended}

在仓库根目录运行：

```sh
cargo fetch --locked
cargo xtask dev app
```

此命令构建并运行原生桌面客户端，使用当前检出的 `.dev/paseo-home` 和端口 `6768`，不会替换已安装的应用和 daemon。优化构建使用 `cargo xtask build-flutter linux`、`macos` 或 `windows`，须在对应主机执行。构建与打包产物位于 `artifacts/flutter/`；本地构建不等于已发布的签名安装包。

## 服务器 / CLI {#server--cli}

构建原生运行时，并保留可执行文件旁的全部资源：

```sh
cargo fetch --locked
cargo xtask build-daemon
./target/debug/paseo onboard
./target/debug/paseo daemon start
./target/debug/paseo daemon status
```

Windows 使用 `paseo.exe`；设置 `CARGO_TARGET_DIR` 时使用对应路径。`cargo xtask build-daemon --release` 在 release 目标目录生成优化的本地运行时；优化构建本身不赋予正式发布身份。

daemon 通常监听 `ws://127.0.0.1:6767/ws`。配置和持久状态仍在 `PASEO_HOME`（默认 `~/.paseo`），也可通过 `--home` 指定。`paseo onboard` 显式执行设置；直接运行 `paseo` 打开 CLI 聊天，而不是安装 daemon。隔离的开发 daemon 使用 `cargo xtask dev daemon`。

使用原生 daemon 压缩包时，应解压**整个**压缩包，保留可执行文件旁的库、技能、声明和运行时元数据，再运行可执行文件。旧 npm CLI 包不提供此 Rust 运行时。

## 移动客户端 {#mobile-clients}

```sh
cargo xtask build-flutter android --arch arm64
cargo xtask build-flutter ios
```

Android 包名为 `sh.paseo.sessions`。产物在 `artifacts/flutter/`；未配置正式签名或描述文件时，产物明确标记为调试签名或未签名，不是商店提交包。客户端需添加远程 daemon 的可达地址和密码。手机的 localhost 并不是 daemon 主机。

## Docker {#docker}

从当前源码构建仅含原生 daemon 的镜像：

```sh
docker build -f docker/base/Dockerfile -t paseo:local .
docker run -d --name paseo-native \
  -p 127.0.0.1:6767:6767 \
  -e PASEO_PASSWORD=change-me \
  -v "$PWD/paseo-home:/home/paseo" \
  -v "$PWD/workspace:/workspace" \
  paseo:local
```

请设置真实密码。通过原生客户端或 CLI 连接；HTTP 根路径**不提供网页客户端**。Agent CLI 需另外提供。现有镜像仓库中的版本不代表此次原生迁移已发布。

## 运行范围 {#runtime-scope}

原生产品没有网页客户端、Electron/Chromium、内嵌浏览器或浏览器自动化、交互式终端界面，也不执行 React UI 插件。工作区和服务脚本仍在 daemon 管理的 PTY 中运行，客户端显示只读日志。原生后端插件与外部 Rust SDK 仍受支持。

已记录的 Linux 执行证据和未验证的平台边界见仓库 `docs/testing.md`。CI 矩阵定义不等于设备测试通过。原生包不提供语音识别、语音对话或 TTS，保留一般音效播放。

连接与开发详情见[连接](connectivity.zh-CN.md)、[CLI 参考](cli.zh-CN.md)、[Rust SDK](sdk/index.zh-CN.md)，以及源码中的 [README](../README.zh-CN.md) 和[开发指南](../docs/development.md)。
