# Lifecycle logger migration notes

This is a historical JavaScript plugin example, **not an installable native plugin** in the 0.13.1 source checkout. The runtime requires a native executable and does not compile or run the old entries. No Node fallback or executable Flutter UI compatibility is provided.

The former example observed nine events and three before hooks. Port notifications using `Plugin::on`, transforms using `Plugin::before`, and diagnostics using `Context::log` or stderr; stdout is reserved for JSONL IPC. Log only deliberately selected, redacted fields: prompts, history and environment values can be private. `paseo plugin logs <installed-id>` is a recent snapshot, not a live follower.

Use the maintained Rust [workspace-inspector example and exact commands](../../public-docs/plugins/index.md#run-the-repository-example), crate `paseo-workspace-inspector`, for a real native plugin. Its source is [here](../workspace-inspector/src/lib.rs); it demonstrates bounded local-file RPC, settings, lifecycle hooks and permission-gated provider sessions, not every historical example feature.

See the [native SDK reference](../../public-docs/plugins/reference.md), [provider contract](../../public-docs/plugins/providers.md) and [explicit migration guide](../../public-docs/plugins/migration.md). Preserve existing installation data; do not remove/reinstall merely to suppress a legacy-runtime error, because removal deletes settings. These notes do not claim publication, deployment or test execution.
