---
title: 更新
description: 原生构建身份、更新通道与迁移可用性。
nav: 更新
---

# 更新 {#updates}

当前检出版本为 **0.13.1**。请使用[本仓库的发布页面](https://github.com/emotionbug/paseo/releases)及其记录的源代码修订确认产物身份。上游下载、旧 npm CLI 包、应用商店条目和容器 `latest` 标签不能证明此分支的发布身份。

## 原生构建身份 {#native-build-identity}

产品客户端使用 Flutter，目标为 Linux、macOS、Windows、Android 和 iOS。桌面/服务器运行 Rust daemon 与 Rust `paseo` CLI；移动客户端只连接远程 daemon。

本地源码构建需要 Rust/Cargo、Flutter 及目标平台的原生工具：

```bash
cargo fetch --locked
cargo xtask build-daemon
# 优化的本地构建，不代表发布来源身份：
cargo xtask build-daemon --release
cargo xtask build-flutter linux
```

在匹配的桌面宿主系统选择 `linux`、`macos` 或 `windows`。Flutter 产物位于 `artifacts/flutter/`。daemon 位于 `target/debug/paseo` 或 `target/release/paseo`（Windows 为 `paseo.exe`），设置 `CARGO_TARGET_DIR` 时以其为根目录。请保留可执行文件旁生成的全部资源。

## 更新本地安装 {#updating-a-local-installation}

替换运行时前先备份 Paseo 主目录。默认仍为 `~/.paseo`，保留原有配置和历史。使用 `paseo daemon stop` 停止托管 daemon，用目标修订构建的完整运行时替换，然后运行 `paseo daemon start` 和 `paseo daemon status`。重启 worker 不会替换 supervisor 可执行文件。

直接运行 `paseo` 打开 CLI 聊天；显式设置请用 `paseo onboard`，它不是更新命令。

## 通道与发布 {#channels-and-publication}

稳定版和测试版是发布身份，不证明此次迁移已经发布。优化本地二进制文件不等于启用官方更新来源身份。原生发布需要验证源代码身份、通道/仓库元数据、完整运行时资源、适用的签名以及明确发布授权。

本地 Flutter 构建不应沿用旧桌面应用更新界面的操作说明。当前更新清单、灰度控制和打包要求见仓库[发布指南](../docs/release.md)。

## 移动端与验证边界 {#mobile-and-verification-boundaries}

`cargo xtask build-flutter android` 或 `cargo xtask build-flutter android --arch arm64` 构建 Android 远程客户端。没有发布签名凭据时，产物明确标记为调试签名。`cargo xtask build-flutter ios` 需要 macOS/Xcode；没有 provisioning/export 配置时输出未签名的设备应用归档。这些产物不自动具备上架条件。

实际执行记录与平台限制见[测试证据](../docs/testing.md)。历史编译结果或 CI 工作流定义不能证明新版本已安装或在设备上运行。

分发原生包时请保留生成的 Cargo 与捆绑资源声明。声明不是合规认证或对应源码提供承诺，应审查各项依赖义务。具体分发要求见[发布指南](../docs/release.md)。不再包含语音运行时或模型下载。
