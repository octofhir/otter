# AGENTS.md

Guidance for coding agents (Claude Code / Codex CLI / etc.) when working in this repository.

## Project Overview

Otter is an embeddable TypeScript/JavaScript engine for Rust applications built on a custom bytecode VM. It provides a safe runtime for executing TypeScript/JavaScript code with native Rust integration, plus a standalone CLI.

**Workspace naming:** the workspace crate is `otterjs`, but it builds the `otter` binary (`crates/otterjs/Cargo.toml`).

> **Note:** The VM is under active development. Some features (full Web APIs) are being added incrementally.

## Migration Status

The target runtime stack is:

- `crates/otter-gc`
- `crates/otter-vm`
- `crates/otter-runtime`
- `crates/otter-jit`

The legacy stack is frozen and must be retired incrementally:

- `crates/otter-engine`
- `crates/otter-vm-runtime`
- `crates/otter-vm-core`
- any crate that still requires them transitively

Migration policy:

- New runtime, VM, Web API, extension, Node.js API, FFI, KV, and SQL work must target the new stack.
- Do not introduce new dependencies from target-stack crates to legacy crates.
- Port in vertical slices; temporary coexistence is allowed, but dependency direction must always move toward the new stack.
- If a legacy crate is no longer referenced by any live workspace member, remove it from `[workspace].members` so it stops participating in `cargo check`, `cargo test`, and `cargo build`.
- Removing a crate from `[workspace].members` is not sufficient if an active crate still depends on it by path; Cargo will still compile it.

## Agent Checklist (per task)

1. **Confirm intent + constraints**: Web API compatibility? sandbox/permissions? performance target? platform?
2. **Check ES conformance status**: Before working on feature implementation or bug fixes, consult `ES_CONFORMANCE.md` to understand the current pass rate for the affected area.
3. **Search before adding**: prefer `rg` to find similar code and reuse existing patterns.
4. **Keep patches surgical**: avoid refactors unless requested; keep public APIs stable.
5. **Respect safety boundaries**: follow the `unsafe` rules and GC invariants below.
6. **Update the "triangle" when needed**: runtime behavior ↔ TypeScript `.d.ts` ↔ docs/examples/tests.
7. **Parse JS/TS with ASTs**: use `oxc`/SWC; never regex-parse JS/TS.
8. **Protect the migration boundary**: no new imports from target-stack crates into legacy crates, and no new imports from target-stack crates back to legacy crates.
9. **Prefer build-graph cleanup**: when a migration slice lands, check whether any legacy crate can be removed from the active workspace/build graph immediately.

## Repository Map (where to change what)

### Target Runtime Stack
- `crates/otter-gc`: target garbage collector.
- `crates/otter-vm`: target VM, interpreter, value model, intrinsics, source compiler.
- `crates/otter-runtime`: target public runtime and embedding surface.

### Supporting crates
- `crates/otter-jit`: JIT pipeline for the target VM and an active part of the new stack.
- `crates/otter-macros`: `#[dive]` proc-macro for native function bindings.
- `crates/otter-nodejs`: Node.js API compatibility layer to be ported onto the target stack.
- `crates/otter-pm`: package management + bundled type definitions (`@types/otter`).
- `crates/otter-modules`: active home for otter-specific hosted modules on the target stack (`otter:kv`, `otter:sql`, `otter:ffi`, and similar surfaces).
- `crates/otter-web`: active home for standards-facing Web API surfaces on the target stack (`TextEncoder`, `TextDecoder`, future `URL`, `fetch`, etc.).
- `crates/otterjs`: CLI (`otter`) and config (`otter.toml`).

### Legacy Stack (frozen; port away from it)
- `crates/otter-engine`
- `crates/otter-vm-runtime`
- `crates/otter-vm-core`
- `crates/otter-vm-gc`
- `crates/otter-node-compat`

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

### Intrinsic and Bootstrap Pattern

New ECMAScript builtins, global namespaces, Web API globals, and extension-visible host objects must follow the target-stack descriptor/builder/bootstrap flow described in `OTTER_VM_SPEC_PLAN.md`.

- Add new bootstrap work in `crates/otter-vm` / `crates/otter-runtime`, not in legacy crates.
- Keep global installation centralized; do not scatter ad-hoc global mutation across unrelated modules.
- Prefer descriptor/builder style APIs over one-off registration functions when exposing JS-visible constructors, prototypes, and namespaces.
- If a feature still exists only in the legacy stack, port it; do not extend the legacy bootstrap surface further.

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

- Timers are target-runtime primitives, not Node-specific APIs:
  - `setTimeout`, `setInterval`, `setImmediate`, `queueMicrotask` must belong to the target stack.
  - `node:timers` / `timers` must re-export target runtime globals, not maintain a separate backend.
- Promise settlement for async native APIs must go through the target VM/runtime job queue.
- Worker tasks may execute plain Rust async work, but VM/JS interaction must hop back onto the target runtime's scheduling boundary.
- For stream/iterator-like async APIs, use explicit pending queues and deterministic delivery semantics.
- When porting async code from the legacy stack, move semantics first and adapter glue second; do not preserve legacy runtime abstractions just for compatibility.

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
- Run target VM tests: `cargo test -p otter-vm`
- Run target runtime tests: `cargo test -p otter-runtime`
- Run a single support crate after porting work there: `cargo test -p otter-nodejs`, `cargo test -p otter-modules`, etc.

## Architecture

### Crate Hierarchy (bottom to top)

```
otterjs (CLI -> `otter`)
    ↓
target host/runtime integration layer
    ↓
otter-runtime
    ↓
otter-vm
    ↓
otter-gc
```

Supporting crates:
- `otter-macros` - `#[dive]` proc-macro for registering native Rust functions callable from JS
- `otter-nodejs` / `otter-modules` - support crates on or moving onto the target runtime
- `otter-pm` - NPM package manager integration

### Key Architectural Constraints

1. **GC Safety**: Values must be properly rooted when stored across GC boundaries. Use the target stack's GC/reference types and rooting patterns; do not introduce new long-lived GC ownership patterns only in legacy crates.

2. **Value Representation**: The target value model lives in `crates/otter-vm/src/value.rs`. Do not add new JS-visible value ABI work to legacy crates.

3. **Object Model**: The target object model lives in `crates/otter-vm/src/object.rs`. Object semantics, property behavior, and host object integration should move there over time.

4. **Async ops require Tokio**: async ops are scheduled onto a Tokio runtime handle (thread-local).

5. **TypeScript Pipeline**: Compilation via oxc parser. Type checking via tsgo (to be re-enabled).
6. **AST-only parsing**: Use ASTs via `oxc` for JS/TS analysis or transforms; no regex parsing.

### Builtin Functions

Native functions are registered via runtime/engine extensions. Example:
```rust
use otter_vm::descriptors::VmNativeCallError;
use otter_vm::value::RegisterValue;

pub fn console_log(args: &[RegisterValue]) -> Result<RegisterValue, VmNativeCallError> {
    for arg in args {
        print!("{arg:?}");
    }
    println!();
    Ok(RegisterValue::undefined())
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
  - Async trace parent/count validation: run the relevant target-runtime profiling tests once that coverage lives on the target stack.

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

Capability-based, deny-by-default. During migration this may still be implemented partly in legacy crates, but all new permission work must target the new runtime integration layer:
- `fs_read`, `fs_write` - Path allowlists
- `net` - Host allowlists
- `env` - Variable allowlists with built-in deny patterns for secrets (AWS_*, *_SECRET*, etc.)
- `subprocess`, `ffi` - Boolean flags

Practical rules when adding/altering APIs:
- **Never bypass capabilities**; enforce checks in the Rust boundary and cover with tests.
- **Env access must stay isolated**: preserve default deny behavior and secret deny patterns while porting env access to the target stack.

## TypeScript / Types

- Bundled types live in `crates/otter-pm/src/types/` and get installed into `node_modules/@types` for editor resolution.
- `crates/otter-pm/src/types/otter/` is the source of truth for Otter `.d.ts` files.
- `packages/otter-types/` is a publish artifact and should be generated from that source, not edited independently.
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

- VM tests: `cargo test -p otter-vm`
- Runtime tests: `cargo test -p otter-runtime`
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
cargo test -p otter-vm
cargo test -p otter-runtime
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
