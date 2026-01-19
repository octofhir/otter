# CLAUDE.md

Guidance for coding agents (Claude Code / Codex CLI / etc.) when working in this repository.

## Project Overview

Otter is an embeddable TypeScript/JavaScript engine for Rust applications built on JavaScriptCore (JSC). It provides a safe, async runtime for executing TypeScript/JavaScript code with native Rust integration, plus a standalone CLI.

**Workspace naming:** the workspace crate is `otterjs`, but it builds the `otter` binary (`crates/otterjs/Cargo.toml`).

## Agent Checklist (per task)

1. **Confirm intent + constraints**: Node/Web API compatibility? sandbox/permissions? performance target? platform?
2. **Search before adding**: prefer `rg` to find similar code and reuse existing patterns.
3. **Keep patches surgical**: avoid refactors unless requested; keep public APIs stable.
4. **Respect safety boundaries**: follow the `unsafe`/FFI rules and JSC threading invariants below.
5. **Update the “triangle” when needed**: runtime behavior ↔ TypeScript `.d.ts` ↔ docs/examples/tests.

## Repository Map (where to change what)

- `crates/otter-jsc-sys`: raw JavaScriptCore FFI + platform linking + bun-webkit downloader (audit boundary for `unsafe`).
- `crates/otter-jsc-core`: safe RAII wrappers around JSC primitives (`JscContext`, `JscValue`, exceptions, GC protection).
- `crates/otter-runtime`: event loop, extension system, Web APIs (fetch/timers/etc), TypeScript transpile (SWC), tsgo type-checking, JS shims (`bootstrap.js`, `commonjs_runtime.js`).
- `crates/otter-engine`: module loader/graph, capabilities (permissions), isolated env store, import maps, remote module allowlists.
- `crates/otter-node`: Node.js compatibility layer (built-in modules + JS wrappers + Rust ops).
- `crates/otter-pm`: package management + bundled type definitions (`@types/otter`, `@types/node`).
- `crates/otterjs`: CLI (`otter`) and config (`otter.toml`) and watch/HMR.

## Development Philosophy

- **Production-ready code**: No premature micro-optimizations. Write clean, idiomatic Rust first.
- **Performance target**: Match or exceed Node.js and Deno performance; approach Bun where possible.
- **API compatibility**: Prioritize compatibility with Node.js APIs and web standards.
- **Idiomatic Rust**: Follow Rust best practices, use proper error handling, leverage the type system.
- **Secure defaults**: deny-by-default permissions; new capabilities must be explicit and testable.

## Build Commands

```bash
# Build
cargo build                          # Debug build
cargo build --release -p otterjs     # Release CLI binary

# Test
cargo test --all --all-features      # Run all tests

# Lint
cargo fmt --all                      # Format code
cargo clippy --all-targets --all-features -- -D warnings

# Run scripts
cargo run -p otterjs -- run <file>   # Run a script
cargo run -p otterjs -- check <file> # Type check with tsgo

# Quick local loop
just fmt && just lint && just test
```

Justfile shortcuts available: `just fmt`, `just lint`, `just test`, `just build`, `just release`

Fast iteration tips:

- Run a single crate: `cargo test -p otter-runtime` / `cargo test -p otter-engine` / `cargo test -p otter-node`
- Run examples: `just examples` or `just example-ts basic` / `just example-js basic`

## Architecture

### Crate Hierarchy (bottom to top)

```
otterjs (CLI -> `otter`)
    ↓
otter-node (Node.js API compatibility)
    ↓
otter-engine (ESM loader, module graph, capabilities)
    ↓
otter-runtime (event loop, extensions, transpiler)
    ↓
otter-jsc-core (safe RAII wrappers)
    ↓
otter-jsc-sys (raw FFI bindings - ALL unsafe code here)
```

Supporting crates:

- `otter-macros` - `#[dive]` proc-macro for registering native Rust functions callable from JS
- `otter-pm` - NPM package manager integration

### Key Architectural Constraints

1. **Thread Safety**: `JscContext` and `JscValue` are `!Send + !Sync`. JSC contexts must stay on the thread that created them. The `EngineHandle` is `Send + Sync` for cross-thread job submission via channels.

2. **FFI Boundary**: All unsafe FFI lives in `jsc-sys`. The `jsc-core` crate provides safe wrappers. Every `unsafe` block must have a `// SAFETY:` comment. See `FFI_SAFETY.md` for the full review checklist.

3. **GC Protection**: Any `JSValueRef` stored across API boundaries must be protected with `JSValueProtect` and unprotected on drop.

4. **Async ops require Tokio**: async ops are scheduled onto a Tokio runtime handle (thread-local). If you add/modify async ops, ensure the caller sets a handle (CLI does via `set_tokio_handle(Handle::current())`).

5. **TypeScript Pipeline**: Type checking via tsgo (10x faster than tsc), transpilation via SWC.

### Node.js Module Implementation Pattern

Each Node.js module in `otter-node/src/` follows this pattern:

- `<module>.js` - JavaScript wrapper that calls native functions with prefixed names
- `<module>_ext.rs` - Rust implementation registering ops via the Extension system

Built-in modules should register exports via `__registerModule(...)` so they can be loaded via both `node:<name>` and bare `<name>` (see `crates/otter-runtime/src/bootstrap.js`).

## Extension System (how host ops reach JS)

- `Extension` is a **bundle** of ops + optional JS setup code.
- Each op name becomes a **global function** in the JS context (avoid collisions; prefix when needed).
- JS setup code (`Extension::with_js(...)`) runs after ops are registered and typically:
  - creates a JS API surface (classes/functions),
  - and calls `__registerModule(name, exports)` for builtins.

Minimal pattern:

```rust
use otter_runtime::{Extension, extension::{op_sync, op_async}};
use serde_json::json;

Extension::new("example")
  .with_ops(vec![
    op_sync("__otter_example_ping", |_ctx, _args| Ok(json!("pong"))),
    op_async("__otter_example_sleep", |_ctx, _args| async move { Ok(json!(true)) }),
  ])
  .with_js(include_str!("example.js"))
```

## Platform Support

- **macOS**: default is **bun-webkit (JIT enabled)**, auto-downloaded by `crates/otter-jsc-sys/build.rs` and cached under `$CARGO_HOME/cache/bun-webkit` (or `~/.cargo/cache/bun-webkit`).
  - To speed up local builds (but disable JIT / slower runtime), set `OTTER_USE_SYSTEM_JSC=1` to use the system JavaScriptCore framework.
- **Linux/Windows**: bun-webkit (statically linked) is auto-downloaded/cached by `crates/otter-jsc-sys/build.rs`.

Useful env vars:

- `OTTER_USE_SYSTEM_JSC=1` (macOS only): use system JSC (no download, no JIT).
- `BUN_WEBKIT_VERSION=...`: override the bun-webkit release hash used by the downloader.

## Debugging

- Logs: CLI uses `tracing`; try `RUST_LOG=debug cargo run -p otterjs -- run examples/basic.ts`.
- Long-running scripts/servers: use `--timeout 0` (disables the timeout).
- When editing embedded JS shims: they are compiled in via `include_str!` and passed through `CString::new(...)` (no `\0` bytes).

## Security Model

Capability-based, deny-by-default:

- `fs_read`, `fs_write` - Path allowlists
- `net` - Host allowlists
- `env` - Variable allowlists with built-in deny patterns for secrets (AWS_*, *_SECRET*, etc.)
- `subprocess`, `ffi` - Boolean flags

Practical rules when adding/altering APIs:

- **Never bypass capabilities** in JS wrappers; enforce checks in the Rust op boundary (or earlier) and cover with tests.
- **Keep `fetch()` deny-by-default**: `otter-runtime` requires `set_net_permission_checker(...)` to be set; it cannot be “cleared” (OnceLock).
- **Env access must stay isolated**: use `otter-engine`’s `IsolatedEnvStore` / `EnvStoreBuilder` (default deny + deny patterns).

## TypeScript / Types

- Type checking uses **tsgo** (auto-downloads if missing; cached under `dirs::cache_dir()/otter/tsgo/v{version}/`).
- Bundled types live in `crates/otter-pm/src/types/` and get installed into `node_modules/@types` for editor/tsgo resolution.
- If you add a new global API or built-in module surface, update the corresponding `.d.ts` file(s).
- If you change tsgo integration, run ignored tests: `cargo test -p otter-runtime -- --ignored`.

## CLI Notes

- Default config file search: `otter.toml`, `otter.config.toml`, `.otterrc.toml` (walks up parent dirs).
- Permissions flags are additive/overriding: `--allow-read/--allow-write/--allow-net/--allow-env`, plus `--allow-run` and `--allow-all`.
- Direct run is supported: `cargo run -p otterjs -- path/to/script.ts` (no `run` subcommand).

## Benchmarks

- Runtime microbenchmarks: `cargo bench -p otter-runtime`
- HTTP benchmark scripts: `./run-benchmark.sh` (Otter) and `./run-bun-benchmark.sh` (Bun) require `k6`.

## Key Files

- `ARCHITECTURE.md` - Detailed threading model, extension system, module loading
- `FFI_SAFETY.md` - Unsafe code guidelines and review checklist
- `ROADMAP.md` - Feature status and API compatibility matrix
