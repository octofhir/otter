# Engine Refactor Execution Plan

This is the living plan for the remaining engine work. It is intentionally
breaking-change friendly: remove weak abstractions instead of preserving
compatibility shims.

Primary product goal: make the `otter` CLI a practical Node.js replacement as
soon as possible for package install, package/script/bin execution, TypeScript
entrypoints, ESM loading, permissions, diagnostics, and profiling.

Active stack:

```text
otter-gc -> otter-vm -> otter-runtime -> product crates
```

Do not add parallel runtime stacks, third-party JS engines, JIT work, or
dependencies from active crates into `crates-legacy/*`.

## Non-Negotiable Invariants

- Runtime/VM work happens on the isolate/runtime boundary.
- Host/Tokio work carries only owned host data and returns through typed runtime
  messages or narrow host services.
- No VM state, `Value`, `Interpreter`, `NativeCtx`, handle scopes, GC handles,
  or raw `BytecodeModule` references cross into host tasks.
- `ExecutionContext` is the only accepted context carrier for runtime async JS
  dispatch; do not reintroduce `current_module`.
- Do not expose `Rc<BytecodeModule>` across public runtime boundaries.
- `otter-runtime` owns capability checks, loader decisions, source maps,
  diagnostics, module graph state, and package-graph integration.
- `otter-vm` must not know package managers, registries, lockfiles, filesystem
  traversal policy, network policy, or product APIs.
- Parse JS/TS with ASTs (`oxc`/SWC). Never regex-parse JavaScript or
  TypeScript.
- Keep the CLI path working after every slice.

## Current Boundary Status

- `current_module` is removed.
- `ExecutionContext::module()` is removed.
- `NativeCtx` carries `Option<ExecutionContext>`, not raw module state.
- Promise reactions, dynamic import jobs, timers, and finalization cleanup jobs
  carry `ExecutionContext`.
- Timer host wakeup is token/message based; the public `TimerCallback` and
  arbitrary timer closure API are gone.
- `event_loop` is internal to `otter-runtime`; `TokioEventLoop`, `EventLoop`,
  timer tokens/requests, host futures, and wake sinks are not root public API.
- Runtime no longer stores a raw `tokio::runtime::Handle`.
- HTTPS dynamic-import fetch runs on the host side and returns owned source text
  through a runtime inbox message before isolate-side compile/evaluate/settle.
- Active runtime installs `process` from a focused runtime module:
  `argv`/`argv0`/`execArgv`/`execPath`/`platform`/`version`/`versions`/`cwd`
  exist, and `env` is materialized through env capability checks plus the
  built-in secret denylist.
- Package scripts run with `project/node_modules/.bin` prepended to `PATH`.

## P0: CLI As Node Replacement

These are the highest-priority remaining slices. Do them before polishing
secondary embedding APIs.

### P0.1 Run Path Reliability

- Make `otter run <target> [args...]` the single production execution path for:
  file entries, package scripts, and package bins.
- Keep direct file shorthand wired to the same runtime/session path as
  `otter run`.
- Ensure script/bin args and environment match Node-compatible CLI behavior.
- Add fixture coverage for:
  - TS entrypoint with imports;
  - package script;
  - local package bin;
  - workspace package import;
  - JSON module;
  - failing script with stable diagnostic.

Acceptance:

- `cargo check -p otter-cli`
- focused CLI run/check/test fixture tests
- no direct VM bypass in CLI execution

### P0.2 Package Install And Graph Usability

- Make `otter install` reliable for normal npm-style projects:
  registry package, workspace package, file dependency, tarball dependency, bin
  linking, lifecycle policy, and stable `otter.lock`.
- Keep migration path from npm/pnpm lockfiles read-only on input and native
  `otter.lock` on output.
- Keep package-manager commands trusted CLI operations; do not require runtime
  capability flags for explicit install/add/remove/outdated/init.
- Runtime module loading must read the package graph, never mutate installs.

Acceptance:

- install from scratch, second install no-op, lockfile byte-stable
- package bin resolves via `otter run <bin>`
- installed bare imports resolve through runtime package graph

### P0.3 Permissions And Node-Compatible Host APIs

- Default deny for runtime capabilities stays intact.
- Prioritize the Node replacement minimum:
  - practical `process` bootstrap with env capability gating;
  - filesystem read/write with path allowlists;
  - timers and microtasks;
  - dynamic import with file and HTTPS capability gates;
  - useful `console`;
  - package bin/script subprocess policy.
- Every host API must validate permissions on the isolate/runtime boundary,
  copy owned host data, run host work outside VM state, then settle on the
  isolate thread.

Acceptance:

- deny-by-default tests for fs/env/net/subprocess
- allowed-path/allowed-host positive tests
- no host task captures VM state, enforced by compile-fail/audit tests

## P1: Runtime Correctness

### P1.1 Module Semantics

- Finish ESM live bindings with an explicit export-binding model, not only
  object-property indirection.
- Implement indirect export resolution per ECMA-262.
- Keep cyclic module evaluation stable.
- Define CJS interop policy after ESM semantics are solid.

Acceptance:

- targeted `language/module-code` Test262 delta improves
- cycle/live-binding fixtures remain green

### P1.2 Function And Builtin Semantics

- Finalize Function object layout and metadata:
  name, length, constructability, receiver handling, native error labels.
- Convert remaining ECMAScript builtins to static descriptor specs through the
  builder/bootstrap path.
- Bare constructors (`Array`, `Number`, `Boolean`, `String`, `JSON`, `Math`,
  `Function`, `Object`) must surface with the expected callable/constructor
  shape and descriptors.
- Add per-family Test262 baselines before semantic changes so deltas are
  attributable.

Acceptance:

- `built-ins/Function`, `built-ins/Object`, and relevant constructor family
  filters do not regress
- global bootstrap snapshot stays intentional and reviewed

### P1.3 Error And Diagnostic Quality

- Stable diagnostic codes for resolver, compile, capability, runtime throw,
  timeout, OOM, package install, and lifecycle failures.
- Pretty and JSON diagnostics must include source URL, range where available,
  cause chain, and actionable context.
- CLI should prefer standard machine-readable outputs for trace/profiling.

Acceptance:

- diagnostic snapshot tests for common CLI failure modes
- no ad-hoc string-only errors on public CLI paths

## P2: Performance And Production Hardening

- Keep hot-path runtime metadata static: no per-call dynamic registry parsing.
- Continue removing public APIs that expose VM implementation details.
- Add focused CPU profiles for package/script startup, module loading, JSON,
  object/property operations, and common builtins.
- Keep tracing/profiling default-off and Chrome/Perfetto/DevTools compatible.
- Audit blocking operations on the isolate thread; host I/O must move behind
  typed services or messages.

Acceptance:

- startup and module-load profiles have before/after artifacts when optimized
- `tokio_spawn_audit` stays current
- no new broad host future/closure public API

## Required Checks By Slice

Always run the narrowest relevant set plus the CLI check:

```bash
cargo check -p otter-runtime
cargo check -p otter-cli
```

For event loop, promises, timers, or dynamic import:

```bash
cargo test -p otter-runtime --test microtask_ordering
cargo test -p otter-runtime --test cross_thread_promise_settlement
cargo test -p otter-runtime --test module_dynamic_import_capability
cargo test -p otter-runtime --test tokio_spawn_audit
```

If `module_dynamic_import_capability` fails with `bind: Operation not
permitted`, rerun it with elevated permissions; it starts a local listener.

For VM semantic changes:

```bash
cargo test -p otter-vm --lib
```

For Test262-backed changes, record before/after targeted pass rates in the PR
or commit notes.
