# Coding Standards

The core instinct: AI-generated code hedges — it covers every case, layers over instead of cutting in, scatters uncertainty everywhere, wraps in case. A senior engineer commits — to a shape, a boundary, a name, a happy path, a type — and lets everything else fall into place. Every rule below catches a different form of indecision.

For testing rules, see [testing.md](testing.md).

## Core principles

- **Zero complexity budget** — every abstraction must justify itself with a specific, current benefit.
- **YAGNI** — build features and abstractions only when needed. A function called once is indirection, not abstraction.
- **No "while I'm at it" cleanups** — make the change you came for. Drive-by edits hide in the diff.
- **Functional and declarative** over object-oriented.
- Keep one deliberate public API per Rust crate or Dart library; do not add pass-through modules that only hide the owner.

## Shell scripts

- Bash scripts always use `#!/usr/bin/env bash`. Never hard-code `/bin/bash` or `/usr/bin/bash`; those paths are not portable to environments such as NixOS.

## Comments and noise

- Delete any comment where removing it loses zero information. Comments explain _why_, not _what_.
- No tutorial comments explaining language features (`// Use destructuring to...`).
- No decorative section dividers (`// ===== Helpers =====`). Use files and modules to organize, not ASCII art.
- No hedging comments (`// might need to revisit`, `// should work for most cases`). If you're unsure, investigate.
- No commented-out code. Git remembers.
- Remove temporary debug output and breakpoints. No `TODO: implement` stubs — if it needs to exist, write it.

## Confidence: commit to a shape

- Validate at boundaries (network, IPC, user input, file I/O), trust types internally. After the parse, the value is what its type says.
- Resolve optionality at the owning boundary instead of repeating fallback chains downstream.
- No defensive checks for conditions the type system already rules out.
- Do not catch errors "just in case." Handle a defined failure or propagate it with context.
- Optionality is a design decision, not a migration shortcut. Represent distinct states with enums, optional data explicitly, and preserve the wire distinction between missing and null values where required.

## Types

- Use Rust structs/enums and Dart model types after boundary validation; avoid untyped maps for known internal shapes.
- Keep one canonical type per concept. Import shared protocol types instead of maintaining parallel copies.
- Name multi-property input/output shapes rather than hiding their meaning in tuple positions or anonymous maps.
- Use enums when a value comes from a known set; use checked conversions where truncation or invalid values matter.
- Use a named options struct when several flags or optional arguments would make call sites ambiguous.
- Make impossible states impossible rather than combining independent loading/error/data flags.

## Errors

- Return typed Rust errors or throw typed Dart exceptions with the fields consumers need. Keep error codes and structured details at public boundaries.
- Handle only failures the caller can resolve; propagate other errors with context. Never turn an unknown failure into an empty success value.
- Separate user-facing copy from log/debug strings — don't make one string serve telemetry, logs, and the UI.
- Fail explicitly. If the caller asked for X and X isn't available, throw — don't silently substitute Y.
- Every fallible user action owns explicit pending, success, and failure UI. Console logs and unverified platform alerts do not satisfy this contract. See [testing.md](testing.md#failure-and-security-expectations).
- A capability advertised to a client means the current runtime can perform the action, not merely that its RPC handler exists. If an unavailable action needs explanatory UI, send the runtime fact or reason separately and keep the server-side refusal fail-closed.

## Density

- Avoid nested conditional expressions. Name branch decisions when a compact expression hides their meaning.
- Boolean expressions with 2+ clauses or mixed concerns → name the conditions.
- Struct and object construction assemble precomputed values; do not hide state transitions in field initializers.
- Break dense transformation chains into named intermediate concepts when they obscure ownership or error handling.
- Keep control flow shallow. Extract a real responsibility rather than adding wrappers solely to reduce indentation.

## Structure and modules

- A directory is a module, not a namespace. One intentional public surface; internal files stay internal.
- Path is part of the name — prefer `provider/registry.rs` over `provider/provider_registry.rs`. If the filename has to do double duty, deepen the path.
- Filenames ending in `-utils`, `-helpers`, `-manager`, `-handler`, `-controller`, `-formatter`, `-builder` are a smell — the path didn't carry enough domain.
- Boundary returns answer the caller's question (`getActiveAgents()`), not "here's my storage" (`getAgents().filter(...)` repeated everywhere).
- One adapter means a hypothetical seam; two adapters means a real one. Don't define a port until something actually varies across it.
- Pass-through modules fail the deletion test — if removing the module makes callers go straight to what they wanted, delete it.
- Centralize policy. The same discriminator (`plan`, `provider`, `kind`, `status`) branched in 3+ files → policy table, not another `else if` per case.
- New features get a home before implementation. A feature smeared across 5 shared files is the same slop as a flat-peer namespace.
- Don't drop new files at the nearest root just because placement is unclear — say so and ask.

## Refactoring is a bolt-on test

- A change should look like a thoughtful edit to existing code, not a new layer next to it. New coordinator wrapping a coordinator, new flag bypassing the normal path, new helper duplicating an existing selector — stop and reshape instead.
- Refactors preserve behavior by default. No removing features to simplify code without explicit approval.
- Have a verification plan _before_ refactoring — name the invariants, confirm a test holds them, write one if not. See [testing.md](testing.md).
- Migrate all callers and remove old paths in the same refactor. No fallback behavior unless explicitly designed.

## Native runtime

Rust owns daemon/CLI execution and Flutter/Dart owns the native client. Keep process/PTY/storage authority on the daemon, protocol validation at each language boundary, and UI lifecycle separate from provider lifetime. Avoid avoidable allocations and whole-history work on hot paths. Do not introduce Node, Electron, Chromium, a webview or a React UI-plugin host as a fallback.

## Naming

- Names describe meaning, not mechanics. `submitForm` over `handleOnClickButtonSubmit`. `running` over `filteredArrayOfRunningAgents`.
- The right length is the shortest unambiguous in context. Inside `AgentManager`, methods are `start`, `stop`, `list`.
- Match the surrounding code's vocabulary. If the codebase uses `getX`, don't introduce `fetchX` / `retrieveX` for the same shape.
- Don't leak implementation into names — `getAgent`, not `queryPostgresForAgent`. If swapping the impl would force a rename, the name is wrong.
- Booleans read as yes/no questions: `isX`, `hasX`, `canX`. Avoid negative booleans (`isNotConnected`).
- `data`, `result`, `info`, `manager`, `temp` are smells — say what the thing _is_.
