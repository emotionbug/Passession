---
title: 分发插件
description: 以 Git 源码、Cargo crate 或校验和固定发行归档分发原生插件。
nav: 发布
---

# 发布插件 {#publish-a-plugin}

从一个可用的[原生插件](index.zh-CN.md)开始。本文说明 **0.13.1 源码检出的分发契约**，不表示 Paseo、SDK crate、注册表条目或你的插件已经发布。不再有 npm 获取或 JavaScript 运行时回退。使用原生 Git 源码、Cargo crate 或 HTTPS 发行归档；注册表记录可选择这些来源。

可扩展的只有后端行为：RPC、钩子、设置数据、Provider 和用量。Flutter 不执行插件界面、面板、主题或 React 时间线渲染器。请说明实际功能、支持的操作系统/架构、外部 Agent 依赖和限制。

## 你的展示页 {#your-listing-page}

将简明的 `OVERVIEW.md` 放在 `paseo-plugin.json` 旁，构建/安装细节放在 `README.md`。脚手架会生成两者。分发时包含这些文档、清单、可执行文件或源码/构建输入、许可声明和引用资源。

说明插件功能、账号/配置、数据访问、网络目的地和所需工具。即使会话历史存本地，发送给上游的提示/工具结果仍离开主机。不能暗示有沙箱。披露软件下载，在下载/安装外部工具前获得明确同意，激活和连接测试也一样。

英文和中文概览可分别使用 `OVERVIEW.md`、`OVERVIEW.zh-CN.md`。清单展示元数据可用语言标签为键的 `translations` 提供 `name`/`description`；daemon 执行不会替这些字段本地化或校验注册表发布政策。保持翻译同步，并确认实际注册表的当前接收要求，不要假定旧网站/审查机器人已支持原生产物。

## 图标和截图 {#icons-and-screenshots}

可选 `name`/`description`、PNG `icon` 和 `media` 仍是展示字段：

```json
{
  "id": "review-tools",
  "name": "Review tools",
  "description": "Reviews local changes",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "bin/review-tools", "args": [] },
  "icon": "assets/icon.png",
  "media": ["assets/result.png"],
  "translations": {
    "zh-CN": { "name": "审查工具", "description": "审查本地改动" }
  }
}
```

此例是预编译 Unix 布局，必须在对应路径提供真实二进制，Windows 使用 `.exe`。本地路径相对清单且不得逃出插件目录；`media` 也可用 HTTPS URL。Provider/用量 SVG 有独立的[参考规则](reference.zh-CN.md#providers)，不等同展示 PNG。截图使用示例数据并隐藏隐私；旧客户端截图不能证明 Flutter 支持自定义 UI。

## 通过 GitHub 或 Git 分享 {#share-through-github-or-git}

提交清单、Rust 源码、`Cargo.toml`、`Cargo.lock`、资源和概览。源码分发明确声明准备命令：

```json
{"build":[["cargo","build","--locked","--release","--target-dir","target"]]}
```

`server.executable` 必须匹配实际二进制和输出目录。daemon 主机需要兼容 Rust 工具链、原生构建依赖和 Git。不从锁文件推断命令。初始脚手架不带 `--locked`，因为首次构建前没有锁文件；可复现分发前应提交生成的锁并修改命令。

把本地 SDK 路径依赖替换为包含原生 crate 的可获取 Git 修订并固定该修订，或在确实发布后使用已确认的版本。不要写不存在的 crates.io 依赖或声称 `0.13.1` 已发布。源码归档不能依赖分发项目外的文件。

以下仅展示语法，请替换为真实仓库和审查过的修订：

```sh
paseo plugin install github:acme/paseo-review --ref v1.2.0
paseo plugin install git:https://git.example/acme/review.git --ref v1.2.0
paseo plugin install github:acme/monorepo --path plugins/review --ref v1.2.0
```

私有 Git 使用 daemon 主机的 Git 配置。应审查不可变提交，而不是只信任可变分支名。直接 Git 普通更新跟随默认 HEAD，除非显式 `--ref` 指定目标；初次标签不会冻结以后普通更新。

## Cargo 源码包 {#cargo-source-packages}

原生来源选择器是 `cargo:<crate>[@<exact-semver>]`。不指定版本时选择 crates.io 最新稳定且未撤回版本；显式精确选择可使用预发行版本。获取记录精确版本、注册表校验和及规范下载 URL。这是**源码获取**，不是向全局路径执行 `cargo install`。

发布 crate 必须包含 `paseo-plugin.json`、全部 Rust 源码/资源，以及 `--locked` 所需锁文件。注意 Cargo 包含规则；工作区相对依赖和遗漏资源会让本地可用项目在打包后失败。宿主安全解包、执行声明准备，再校验暂存目录内可执行文件。明确支持的主机目标；Unix 清单不会自动选择 Windows 可执行文件。

实际完成发布并检查包后，用户可安装精确版本，例如 `paseo plugin install cargo:paseo-review@1.2.0`。这只是示例名称/版本，不表示该包可获取。显式更新用 `paseo plugin update review --version 1.2.1`；普通最新版本可先用 `--check` 预览。

## 预编译发行归档 {#prebuilt-release-archives}

按主机系统/架构提供正确原生二进制、`paseo-plugin.json` 和资源。纯预编译包应省略不必要源码构建命令。tar 归档保留 Unix 执行位。可用 `--path` 指定归档中的插件子目录，但不能逃出解包根目录。获取拒绝路径穿越、链接/特殊条目、重复路径和超大内容。

安装契约：

```text
paseo plugin install release:<HTTPS-archive-URL> --version <semver> --sha256 <64-character-lowercase-digest> --format <tar.gz|zip> [--path <plugin-subdirectory>]
```

SHA-256 针对**归档字节**而非仅二进制；从可信发行渠道取得摘要。要求无 URL 凭据的 HTTPS，只有已识别后缀才可省略 `--format`。宿主有界下载、校验摘要、解包、校验，再激活。这能阻止意外替换/路径逃逸，但不证明代码可信，也不沙箱化构建脚本。

发行来源没有“发现最新版本”功能。显式预览下一归档：

```text
paseo plugin update <installed-id> --url <HTTPS-archive-URL> --version <semver> --sha256 <digest> --format <tar.gz|zip> --check
```

只有审查精确目标后才去掉 `--check`；显式选择即授权该目标。URL 必须保持已安装来源的 origin。注册表托管发行跟随审查过的产物目标。普通更新需要 `--yes`；提案绑定精确产物身份，发生变化会失败，不会安装其他内容。

## 插件注册表 {#plugin-registry}

注册表 ID 仍为 `owner/slug`，可带注册表主机前缀。兼容文档必须提供原生来源，而非旧 npm 固定版本：

- Git：沿用审查提交与受限插件路径契约。
- Cargo：`artifact` 包含 `kind: "cargo"`、`crate`、精确 `version`、`checksum`、规范 `resolved` URL、可选 `pluginPath`。
- 发行：`artifact` 包含 `kind: "release"`、HTTPS `url`、`version`、`sha256`、`format`、可选 `pluginPath`。

文档 `id` 应与请求注册表 ID 一致。安装记录保留来源身份与完整产物收据，更新固定到审查过的不可变目标。注册表配置和凭据边界见[来源管理](reference.zh-CN.md#plugin-sources)。

仓库消费者支持新格式，不证明公共注册表已接受 schema、迁移条目或部署接收服务。宣传 `paseo plugin install owner/slug` 前确认原生支持；此期间可直接使用原生来源分发。不要继续使用旧 npm 发布/提交步骤。

## 分发前验证行为 {#validate-behavior-before-distribution}

在隔离测试主机上为声称目标构建，运行可执行文件 `--validate <directory> <daemon-version>`，然后安装并实际覆盖所宣传的 RPC、设置迁移/冲突、钩子、用量发现和 Provider 权限/轮次/持久化路径。活动期间覆盖重新加载/禁用/移除。日志不能包含凭据。校验会运行受信任程序，只检查启动/注册，不证明完整行为或安全。

只声明实际使用过的平台支持，兼容更新保持数据和安装 ID，披露源码/构建/依赖信任。发布与部署需要独立、明确的发行操作；本文不声称这些已经完成。
