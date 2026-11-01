# Rust SDK

[English](index.md)

Paseo 原生 SDK 让 Rust 应用驱动 daemon，保留既有协议与资源 ID。应用负责连接生命周期、JSON 响应处理和订阅清理。

| Crate | 用途 |
| --- | --- |
| `paseo-client` | Tokio 客户端，本地/远程/relay/SSH 传输，Agent/工作区 RPC，订阅、分页及文件/终端二进制传输。 |
| `paseo-protocol` | 无损协议消息、原生验证、Provider 目录、路由、时间线辅助函数和加密传输编解码。 |
| `paseo-highlight` | 原生语法解析、借用 token、主题，以及完整文档结果的分批交付。 |

按[快速开始](quickstart.zh-CN.md)配置仓库路径依赖。工作区基线为 Rust 1.93、edition 2024；这不表示已发布到 crate 仓库。外部 Agent CLI 及其认证仍需单独在 daemon 主机准备。

## 启动 daemon

按照[仓库配置说明](../../README.zh-CN.md)准备开发 daemon。SDK 连接已有 daemon，不会安装、重启或升级它。Provider 操作以该主机的权限在该主机执行。

## 运行 Agent

参见[完整可执行示例](quickstart.zh-CN.md)。公开 API 使用 `DaemonClient`，而不是本地 Agent 句柄集合。创建接受协议形状的 JSON 并返回 JSON payload；后续操作使用返回的 Agent ID。Provider 与模型 ID 是两个独立字段。

## 指南

- [Agent](agents.zh-CN.md)
- [工作区](workspaces.zh-CN.md)
- [Provider](providers.zh-CN.md) 和 [Provider 选项](provider-options.zh-CN.md)
- [事件与历史](events.zh-CN.md)
- [实践示例](recipes.zh-CN.md)
- [API 参考、认证与语法高亮](reference.zh-CN.md)
- [原生协议验证](../../docs/protocol-validation.md)
