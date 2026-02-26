# AGENTS.md

Guidance for coding agents (Claude Code / Codex CLI / etc.) when working in this repository.

## Project Overview

Otter is an embeddable TypeScript/JavaScript engine for Rust applications built on a custom bytecode VM. It provides a safe runtime for executing TypeScript/JavaScript code with native Rust integration, plus a standalone CLI.

**Workspace naming:** the workspace crate is `otterjs`, but it builds the `otter` binary (`crates/otterjs/Cargo.toml`).

> **Note:** The VM is under active development. Some features (full Web APIs) are being added incrementally.

## Agent Checklist (per task)

1. **Confirm intent + constraints**: Web API compatibility? sandbox/permissions? performance target? platform?
2. **Check ES conformance status**: Before working on feature implementation or bug fixes, consult `ES_CONFORMANCE.md` to understand the current pass rate for the affected area.
3. **Search before adding**: prefer `rg` to find similar code and reuse existing patterns.
4. **Keep patches surgical**: avoid refactors unless requested; keep public APIs stable.
5. **Respect safety boundaries**: follow the `unsafe` rules and GC invariants below.
6. **Update the "triangle" when needed**: runtime behavior ↔ TypeScript `.d.ts` ↔ docs/examples/tests.
7. **Parse JS/TS with ASTs**: use `oxc`/SWC; never regex-parse JS/TS.

## Repository Map (where to change what)

### VM Core (new architecture)
- `crates/otter-vm-bytecode`: bytecode instruction definitions and constants.
- `crates/otter-vm-gc`: garbage collector with mark-sweep and generational collection.
- `crates/otter-vm-core`: bytecode interpreter, value representation, objects, strings.
- `crates/otter-vm-compiler`: JS/TS to bytecode compiler using oxc parser.
- `crates/otter-vm-runtime`: runtime and event loop primitives.

### Supporting crates
- `crates/otter-macros`: `#[dive]` proc-macro for native function bindings.
- `crates/otter-engine`: module loader/graph, capabilities (permissions), isolated env store.
- `crates/otter-pm`: package management + bundled type definitions (`@types/otter`).
- `crates/otter-sql`: SQLite + PostgreSQL database support.
- `crates/otter-kv`: key-value store backed by redb.
- `crates/otterjs`: CLI (`otter`) and config (`otter.toml`).

## File Naming Conventions

### Builtin Modules

For builtin modules use the following naming scheme:

| File             | Purpose                                |
|------------------|----------------------------------------|
| `module_ext.rs`  | Rust implementation of native functions |
| `module.js`      | JavaScript shim / polyfills            |

Example for `fs` module:
- `fs_ext.rs` — native functions: `readFile`, `writeFile`, `stat`, etc.
- `fs.js` — JS wrappers, exports, additional logic

This separation:
- Clearly distinguishes Rust and JS code
- Makes it easy to find the right implementation
- Maintains consistency across modules

## Development Philosophy

- **Production-ready code**: No premature micro-optimizations. Write clean, idiomatic Rust first.
- **Performance target**: High-performance execution with competitive benchmarks.
- **API compatibility**: Prioritize compatibility with web standards.
- **AST-first parsing**: Use ASTs via `oxc`/SWC for JS/TS analysis or transforms; do not use regex to parse JS/TS code.
- **Idiomatic Rust**: Follow Rust best practices, use proper error handling, leverage the type system.
- **Secure defaults**: deny-by-default permissions; new capabilities must be explicit and testable.

## Macro and Async Agreements

### Macro usage (`#[dive]`)

- Prefer `#[dive]` for simple native bindings where argument mapping is straightforward and improves readability.
- For module loaders / namespace wiring (`node:*`, profile-gated exports, mixed sync+async APIs), prefer explicit `OtterExtension` + manual module builders instead of hiding behavior behind macros.
- Keep public JS API shape obvious in Rust code: exported names and arity should be visible in one place (`*_ext.rs` module builder).
- If a macro-based API surface changes, update the corresponding tests and `.d.ts` declarations in the same patch.

### Async model

- Timers are runtime primitives, not Node-specific APIs:
  - `setTimeout`, `setInterval`, `setImmediate`, `queueMicrotask` must come from `otter-vm-runtime`.
  - `node:timers` / `timers` must re-export runtime globals, not maintain a separate timer backend.
- Promise settlement for async native APIs must go through JS job queues:
  - Use `JsPromise::with_resolvers(...)` and enqueue through `ncx.js_job_queue()`.
  - Do not settle promises directly from worker threads.
- Worker tasks may execute only plain Rust async work (Tokio); VM/JS interaction must be marshalled back onto VM thread via queued jobs.
- For stream/iterator-like async APIs (e.g. `timers/promises.setInterval()`), use explicit pending queues (for example `VecDeque`) and deterministic delivery semantics.
- In `otter-nodejs` extension code, prefer `std::sync` primitives (`Mutex`, atomics) plus queue handoff; avoid introducing new `parking_lot` usage there unless explicitly justified.

## Common Pitfalls to Avoid

### 1. Wrong Collection Type
**Problem**: Using `HashMap`/`FxHashMap` when output order matters (JSON, iterators).
**Solution**: Use `BTreeMap` or `IndexMap` for deterministic iteration order.

```rust
// JSON object keys must preserve insertion order per spec
use indexmap::IndexMap;
struct JsObject {
    properties: IndexMap<String, Value>,  // NOT HashMap
}
```

### 2. Unbounded Recursion
**Problem**: Stack overflow on deeply nested structures (JSON, AST, objects).
**Solution**: Add depth limits or use iterative algorithms with explicit stack.

```rust
const MAX_NESTING_DEPTH: usize = 512;

fn stringify(value: &Value, depth: usize) -> Result<String, Error> {
    if depth > MAX_NESTING_DEPTH {
        return Err(Error::TooDeep);
    }
    // ... recurse with depth + 1
}
```

### 3. Forgetting GC Roots
**Problem**: Values get collected while still in use.
**Solution**: Root values before operations that might trigger GC (allocations, function calls).

### 4. Non-deterministic Test Failures
**Problem**: Tests pass/fail randomly due to hash map iteration order.
**Solution**: Sort keys before comparison, or use ordered collections throughout.

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
- Run VM tests: `cargo test -p otter-vm-core` / `cargo test -p otter-vm-compiler`
- Run a single crate: `cargo test -p otter-engine`

## Architecture

### Crate Hierarchy (bottom to top)

```
otterjs (CLI -> `otter`)
    ↓
otter-engine (ESM loader, module graph, capabilities)
    ↓
otter-vm-runtime (runtime with builtins)
    ↓
otter-vm-core (interpreter, values, objects)
    ↓
otter-vm-compiler (JS/TS -> bytecode)
    ↓
otter-vm-bytecode (instruction definitions)
    ↓
otter-vm-gc (garbage collector)
```

Supporting crates:
- `otter-macros` - `#[dive]` proc-macro for registering native Rust functions callable from JS
- `otter-pm` - NPM package manager integration

### Key Architectural Constraints

1. **GC Safety**: Values must be properly rooted when stored across GC boundaries. Use `GcRoot<T>` for long-lived references.

2. **Value Representation**: NaN-boxing is used for efficient value storage. See `otter-vm-core/src/value.rs`.

3. **Object Model**: Objects use hidden classes for property access optimization. See `otter-vm-core/src/object.rs`.

4. **Async ops require Tokio**: async ops are scheduled onto a Tokio runtime handle (thread-local).

5. **TypeScript Pipeline**: Compilation via oxc parser. Type checking via tsgo (to be re-enabled).
6. **AST-only parsing**: Use ASTs via `oxc` for JS/TS analysis or transforms; no regex parsing.

### Builtin Functions

Native functions are registered via runtime/engine extensions. Example:
```rust
use otter_vm_core::{Value, VmError};

pub fn console_log(args: &[Value]) -> Result<Value, VmError> {
    for arg in args {
        print!("{}", arg.to_string());
    }
    println!();
    Ok(Value::undefined())
}
```

## Platform Support

Pure Rust implementation - no external JavaScript engine dependencies.

- **macOS**: x86_64, ARM64
- **Linux**: x86_64, ARM64
- **Windows**: x86_64

## Debugging

- Logs: CLI uses `tracing`; try `RUST_LOG=debug cargo run -p otterjs -- run examples/basic.ts`.
- Long-running scripts/servers: use `--timeout 0` (disables the timeout).
- When editing embedded JS shims: they are compiled in via `include_str!` and passed through `CString::new(...)` (no `\0` bytes).
- VM instruction trace for a script (full trace to file):
  - `cargo run -p otterjs -- run <file> --trace --trace-file otter-trace.txt`
  - Optional: `--trace-filter "<regex>" --trace-timing`
  - Use `.json` extension for Chrome Trace format: `--trace-file otter-trace.json`
  - CI/E2E compatibility check: `cargo test -p otterjs trace_e2e_generates_chrome_perfetto_compatible_json`
- CPU profile + folded flamegraph stacks:
  - `cargo run -p otterjs -- run <file> --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Optional: `--cpu-prof-interval 1000 --cpu-prof-name my-run.cpuprofile`
  - Produces both `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph.pl).
  - Stack samples are captured from `VmContext::capture_profiler_stack()` (via runtime debug snapshot), so function/file/line metadata should come from VM frames, not CLI-only reconstruction.
  - Baseline overhead sanity check (`cpu-prof` should stay opt-in): compare `/usr/bin/time -p target/debug/otter --timeout 0 run <file>` vs `/usr/bin/time -p target/debug/otter --timeout 0 --cpu-prof --cpu-prof-dir /tmp/otter-prof run <file>` and watch `real` delta.
  - Script args are forwarded to `process.argv`: `cargo run -p otterjs -- run benchmarks/cpu/flamegraph.ts math 2`
  - Shorthand mode also forwards args: `cargo run -p otterjs -- benchmarks/cpu/flamegraph.ts json 1`
- Async/op trace (Chrome/Perfetto compatible):
  - `cargo run -p otterjs -- run <file> --async-trace --async-trace-file /tmp/otter-prof/run.trace.json`
  - Produces `.trace.json` with `traceEvents` and categories (`timers`, `fetch`, `fs`, `net`, `jobs`, `modules`, `ops`).
  - Async op hops are linked via `args.parentId`/`args.spanId` (dispatch span on VM thread, worker span on async task).
- Combined profiling run (CPU + async trace):
  - `cargo run -p otterjs -- run <file> --timeout 0 --cpu-prof --cpu-prof-dir /tmp/otter-prof --async-trace --async-trace-file /tmp/otter-prof/run.trace.json`
  - Use `--timeout 0` for long benchmarks to avoid default CLI timeout truncating profiles.
- Timeout-focused debug dump (ring buffer snapshot):
  - `cargo run -p otterjs -- run <file> --timeout 20 --dump-on-timeout --dump-file timeout-dump.txt --dump-buffer-size 100`
  - For heavy JSON/object workloads, start with `--dump-buffer-size 10` to keep timeout diagnostics responsive.
  - Trace modified-register values are preview-capped (160 chars) to avoid oversized timeout dumps.
  - Reproducibility guard (stable opcode sequence across repeated interrupted runs): `cargo test -p otterjs timeout_dump_is_reproducible_for_immediate_interrupt`
- Test262 trace workflow:
  - Full trace: `cargo run -p otter-test262 -- run --filter "<pattern>" --trace`
  - Save trace only on failures: `cargo run -p otter-test262 -- run --filter "<pattern>" --trace --trace-failures-only`
  - Save trace only on timeouts: `cargo run -p otter-test262 -- run --filter "<pattern>" --trace --trace-timeouts-only`
- Test262 watchdog dumps (for hangs):
  - Run: `cargo run -p otter-test262 -- --filter <pattern> --verbose --timeout 20`
  - On timeout the runner prints `WATCHDOG: ...` with `stack_depth`, `try_stack_depth`, `pc`, `instruction`, `function_index`, `function_name`, and `module_url`.
  - `module_url=setup-<extension>-<idx>.js` means the hang is in extension JS (e.g., `setup-builtins-1.js` → `builtins.js`, `setup-test262-1.js` → `assert.js`).
  - `module_url=main.js` is the test body.
  - `instruction=` is the bytecode at the current `pc` and helps pinpoint loops or stuck ops.
- Trace schema fields:
  - VM instruction trace JSON includes `otterTraceSchemaVersion` + `traceEvents`.
  - Async trace JSON includes `otterAsyncTraceSchemaVersion` + `traceEvents`.
  - Async trace parent/count validation (runtime async-op burst): `cargo test -p otter-vm-runtime --features profiling test_async_trace_links_parent_child_across_async_hop`

### Debug Workflows (engine improvement)

- Timeout/hang triage:
  - First run timeout dump: `cargo run -p otterjs -- run <file> --timeout 20 --dump-on-timeout --dump-file timeout-dump.txt --dump-buffer-size 100`
  - If workload includes large JSON/string values, retry with `--dump-buffer-size 10` before increasing it.
  - Then narrow with filtered full trace: `cargo run -p otterjs -- run <file> --trace --trace-file otter-trace.json --trace-filter "<module|function>"`
  - Focus on `pc`, `instruction`, and `module_url` to identify VM loop vs extension/bootstrap lockup.
- CPU hotspot triage:
  - Capture profile: `cargo run -p otterjs -- run benchmarks/cpu/flamegraph.ts <mode> <scale> --timeout 0 --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Inspect `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph).
  - Compare hottest frames before/after optimization patches; do not rely only on total runtime.
- Async latency/scheduling triage:
  - Capture async trace: `cargo run -p otterjs -- run <file> --timeout 0 --async-trace --async-trace-file /tmp/otter-prof/run.trace.json`
  - Validate category distribution (`timers`, `jobs`, `fs`, `net`, `fetch`, `modules`, `ops`) and span closure behavior.
  - Use combined run (`--cpu-prof` + `--async-trace`) when stall source is unclear.

### Debug/Profiling Roadmap Rules

- Track all debug/trace/profiling work in `DEBUG_TRACE_PROFILING_PLAN.md`.
- If a patch adds or changes debug/profiling behavior, update:
  1. Runtime behavior (Rust code)
  2. CLI/API surface
  3. `DEBUG_TRACE_PROFILING_PLAN.md` status checkboxes
  4. This `AGENTS.md` section when developer workflow changes
- Keep tooling default-off (minimal overhead unless explicitly enabled).
- Prefer machine-readable outputs (`.trace.json`, `.cpuprofile`, `.heapsnapshot`, `.folded`) over ad-hoc text when adding new tooling.
- Output compatibility is mandatory:
  - `*.trace.json` must follow Chrome Trace Event format (`traceEvents`) for DevTools/Perfetto.
  - `*.cpuprofile` must follow Chrome/V8 profile schema for DevTools/Speedscope.
  - `*.heapsnapshot` must follow Chrome heap snapshot schema for DevTools Memory.
  - `*.folded` must use standard folded stack format for flamegraph tools.
- Do not introduce Otter-only primary profiling formats when a standard format exists.

## Security Model

Capability-based, deny-by-default (via `otter-engine`):
- `fs_read`, `fs_write` - Path allowlists
- `net` - Host allowlists
- `env` - Variable allowlists with built-in deny patterns for secrets (AWS_*, *_SECRET*, etc.)
- `subprocess`, `ffi` - Boolean flags

Practical rules when adding/altering APIs:
- **Never bypass capabilities**; enforce checks in the Rust boundary and cover with tests.
- **Env access must stay isolated**: use `otter-engine`'s `IsolatedEnvStore` / `EnvStoreBuilder` (default deny + deny patterns).

## TypeScript / Types

- Bundled types live in `crates/otter-pm/src/types/` and get installed into `node_modules/@types` for editor resolution.
- If you add a new global API or built-in module surface, update the corresponding `.d.ts` file(s).
- Type checking integration (tsgo) is being ported to the new VM.

## CLI Notes

- Default config file search: `otter.toml`, `otter.config.toml`, `.otterrc.toml` (walks up parent dirs).
- Permissions flags are additive/overriding: `--allow-read/--allow-write/--allow-net/--allow-env`, plus `--allow-run` and `--allow-all`.
- Direct run is supported: `cargo run -p otterjs -- path/to/script.ts` (no `run` subcommand).
- Script argv forwarding is supported in both forms:
  - `cargo run -p otterjs -- run path/to/script.ts arg1 arg2`
  - `cargo run -p otterjs -- path/to/script.ts arg1 arg2`

## Benchmarks

- VM tests: `cargo test -p otter-vm-core`
- Compiler tests: `cargo test -p otter-vm-compiler`
- Test262 conformance: `cargo test -p otter-test262`
- Phase-by-phase cross-runtime baseline (Otter/Node/Bun/Deno): `benchmarks/cpu/phase_baseline.sh`
  - Runs Otter in release mode (`target/release/otter`) and enforces `OTTER_TIMEOUT_SECONDS <= 45` for comparability.
  - Otter perf classification in artifacts:
    - `critical-timeout` = phase hit timeout cap (`45s`)
    - `bad-slow` = phase completed but `phase_ms > 25000`
    - `ok` = phase completed within `<= 25000ms`
  - Writes regression artifacts to `benchmarks/results/`:
    - `PHASE_REGRESSION_DASHBOARD.md`
    - `phase-baseline-<timestamp>.json`
    - `phase-baseline-<timestamp>.tsv`

## Test-Driven Workflow

When implementing features covered by Test262 or Node.js compatibility tests:

### 1. Establish Baseline
```bash
just test262-filter "FeatureName" 2>&1 | grep -E "(passed|failed|Pass rate)"
# Example output: "Pass rate: 39.0% (156/400)"
```

### 2. Fix by Failure Category
Prioritize fixes by impact:
1. Most common error type first (e.g., "TypeError: X is not a function")
2. Then edge cases and spec compliance details

### 3. Track Progress
After each fix, re-run and document the delta:
```bash
# Before: 39.0% (156/400)
# After:  42.4% (170/400)  ← +14 tests passing
```

### 4. Validate No Regressions
Run full test suite after changes to core modules:
```bash
cargo test -p otter-vm-core
cargo test -p otter-vm-runtime
```

## Conformance Tracking

`ES_CONFORMANCE.md` tracks Test262 conformance by ECMAScript edition and by feature area.

### Before starting work

- Look up the relevant section in `ES_CONFORMANCE.md` for baseline pass rates
- Run the targeted test262 subset: `just test262-filter "Array/prototype/map"`

### After completing work

- Re-run tests and note the delta (before/after pass rates)
- If pass rate changed significantly, regenerate:

```bash
just test262-save && just test262-conformance
```

- Include before/after rates in commit message or PR description

### Timeout policy

All test262 runs use a 10-second per-test timeout (hardcoded fallback). Tests that hang
are recorded as `Timeout` in the conformance doc. If you encounter frequent timeouts in
a specific area, investigate for infinite loops before attempting other fixes.

## Key Files

- `ES_CONFORMANCE.md` - ECMAScript conformance status by edition and feature
- `OTTER_VM_PLAN.md` - VM implementation plan and status
- `ROADMAP.md` - Feature status and API compatibility matrix
- `DEBUG_TRACE_PROFILING_PLAN.md` - Debug/trace/profiling implementation tracker
