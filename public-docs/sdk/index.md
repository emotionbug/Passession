# Rust SDK

[中文](index.zh-CN.md)

Paseo's native SDK drives a daemon from your Rust application. It preserves the daemon wire protocol and resource IDs; the application owns connection lifetime, JSON response handling and subscription cleanup.

| Crate | Purpose |
| --- | --- |
| `paseo-client` | Tokio client, local/remote/relay/SSH transports, agent/workspace RPCs, subscriptions, pagination and binary files/terminals. |
| `paseo-protocol` | Lossless wire messages, native validation, provider catalogs, routing, timeline helpers and encrypted transport codecs. |
| `paseo-highlight` | Native syntax parsing, borrowed tokens, themes and incremental delivery of completed-document results. |

Use the checkout path dependencies in [Quickstart](quickstart.md). Rust 1.93 and edition 2024 are the workspace baseline. No registry publication is implied. The external agent CLI and its authentication remain separate prerequisites on the daemon machine.

## Start a daemon

Use the [repository setup](../../README.md) for a development daemon. The SDK connects to an existing daemon and does not install, restart or upgrade it. Provider actions run on that host with that host's permissions.

## Run an agent

Follow the [complete executable example](quickstart.md#create-an-agent). The public API is `DaemonClient`, not a collection of local agent handles. Creation accepts wire-shaped JSON and returns a JSON payload; follow-up calls use the returned agent ID. Provider and model IDs are separate fields.

## Guides

- [Agents](agents.md)
- [Workspaces](workspaces.md)
- [Providers](providers.md) and [provider options](provider-options.md)
- [Events and history](events.md)
- [Recipes](recipes.md)
- [API reference, authentication and highlighting](reference.md)
- [Native protocol validation](../../docs/protocol-validation.md)
