# Provider options

[中文](provider-options.zh-CN.md) · [Setup](quickstart.md)

Provider options are a JSON object (`serde_json::Value`) interpreted by the provider. Claude, Codex and OpenCode validate their accepted keys; unknown or misspelled keys can fail creation. Pi, OMP, ACP and native plugin providers interpret their own options. Do not assume all providers implement the same policy.

Set defaults in `config.json` at `agents.providers.<id>.options`. Per-agent values go in **`config.providerOptions`** in the Rust wire-shaped creation request, not `config.options`. The daemon recursively merges plain objects over defaults; arrays, scalars and null replace their base values. Only per-agent overrides are stored. Default changes take effect at the next launch/resume.

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn launch(client: &DaemonClient, provider: &str, model: &str,
                cwd: &str, provider_options: Value) -> Result<Value> {
    client.create_agent(json!({
        "config": {"provider": provider, "model": model, "cwd": cwd,
                   "providerOptions": provider_options},
        "initialPrompt": "Review the change and report the result."
    }), RequestOptions::default()).await
}
```

Pass a JSON object below to `launch`, for example by constructing it with `serde_json::json!` or parsing a configuration string using `serde_json::from_str`. Use an actual model ID discovered for that provider. Call `wait_for_finish` with the returned agent ID to observe the outcome.

Provider settings constrain an external CLI running as the daemon user. They are not a host security boundary. Run untrusted work in a container or separate machine; see [Security](../security.md).

## Codex

```json
{
  "approval_policy": "never",
  "sandbox_mode": "workspace-write",
  "sandbox_workspace_write": {
    "writable_roots": ["/home/me/dev/storefront"],
    "network_access": false,
    "exclude_slash_tmp": true
  },
  "web_search": "disabled"
}
```

| Option | Accepted values/fields |
| --- | --- |
| `approval_policy` | `untrusted`, `on-request`, `never`, or a `granular` policy object |
| `sandbox_mode` | `read-only`, `workspace-write`, `danger-full-access` |
| `sandbox_workspace_write` | `writable_roots`, `network_access`, `exclude_slash_tmp`, `exclude_tmpdir_env_var` |
| `web_search` | `disabled`, `cached`, `indexed`, `live` |
| `features` | `multi_agent_v2`, `network_proxy` (boolean or proxy/domain policy object) |

`approval_policy: "never"` removes prompts; it does not restrict access. Configure the sandbox deliberately. For example, defaults of `sandbox_mode: "read-only"`, `approval_policy: "never"`, and `sandbox_workspace_write.network_access: false` retain the latter two when an agent overrides only `sandbox_mode` to `workspace-write`.

## Claude

```json
{
  "disallowedTools": ["WebFetch"],
  "sandbox": {
    "enabled": true,
    "failIfUnavailable": true,
    "allowUnsandboxedCommands": false,
    "filesystem": {
      "allowWrite": ["/home/me/dev/storefront"],
      "denyRead": ["/home/me/.ssh", "/home/me/.aws"]
    },
    "network": {
      "allowedDomains": ["crates.io", "github.com"],
      "strictAllowlist": true
    }
  }
}
```

| Option | Meaning |
| --- | --- |
| `allowedTools`, `disallowedTools` | Tools allowed without asking / forbidden tools |
| `additionalDirectories` | Extra accessible directories |
| `sandbox` | `enabled`, `failIfUnavailable`, `allowUnsandboxedCommands`, `excludedCommands`, `filesystem`, `network` |
| `settings` | `permissions` with allow/ask/deny rule lists and nested sandbox settings |

`failIfUnavailable: true` rejects startup if the sandbox cannot be established; leaving it off permits an unsandboxed fallback. Filesystem settings include `allowWrite`, `denyWrite`, `allowRead`, `denyRead`. Network settings include domain allow/deny lists, `strictAllowlist` and proxies. Adapt paths/domains to the daemon host and actual task.

## OpenCode

```json
{
  "permission": {
    "read": "allow", "edit": "allow", "webfetch": "deny",
    "external_directory": "deny",
    "bash": {"git status": "allow", "git diff*": "allow", "git push*": "deny", "*": "ask"}
  }
}
```

Actions are `ask`, `allow`, or `deny`. Target-aware tools (`read`, `edit`, `glob`, `grep`, `list`, `bash`, `task`, `external_directory`, `repo_clone`, `repo_overview`, `lsp`, `skill`) also accept pattern maps. Pattern order matters, including fallback entries. Tools without targets (`todowrite`, `question`, `webfetch`, `websearch`, `codesearch`, `doom_loop`) use a bare action. A bare `permission: "deny"` denies everything.

An `ask` becomes a Paseo permission request. Handle it with an owned event observation and `respond_to_permission`; see [Events](events.md). A waiting run can report `permission`. Do not turn every request into an automatic allow merely to make a run unattended.

## Modes and options

`config.modeId` selects a mode from [provider discovery](providers.md). `config.providerOptions` is native provider configuration; both can be set. Where they overlap, explicit provider options take precedence. For example, `modeId: "full-access"` with Codex `providerOptions: {"approval_policy":"never","sandbox_mode":"read-only"}` remains read-only. Review options rather than inferring permissions from a mode label alone.
