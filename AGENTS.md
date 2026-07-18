# AGENTS.md
For any file search or grep in the current git-indexed directory, use fff tools.

Guidance for coding agents (Claude Code / Codex CLI / etc.) when working in this repository.

## Project Overview

Otter is an embeddable TypeScript/JavaScript engine for Rust applications built on a custom bytecode VM. It provides a safe runtime for executing TypeScript/JavaScript code with native Rust integration, plus a standalone CLI.

**Workspace naming:** the active CLI crate is `otter-cli` under `crates/`; legacy `crates-legacy/*` paths are reference-only unless a task explicitly says otherwise.

> **Note:** The VM is under active development. Some features (full Web APIs) are being added incrementally.

## Runtime Layout

The active runtime stack is:

- `crates/otter-gc`
- `crates/otter-vm`
- `crates/otter-runtime`
- product crates under `crates/*`

Do not introduce parallel engine/runtime stacks through copied modules, renamed
crates, or path dependencies.

Repository rules:

- New runtime, VM, Web API, extension, Node.js API, FFI, KV, and SQL work belongs on the active runtime stack.
- Keep dependency direction simple: `otter-gc` -> `otter-vm` -> `otter-runtime` -> product crates.
- Prefer vertical slices and small ports over large framework rewrites.
- Keep parked compatibility shims out of the active workspace build graph.

## Module Size And Boundary Hygiene

Do not grow kilometer-long `lib.rs` files. New non-trivial runtime,
compiler, resolver, package-manager, or diagnostics behavior belongs in a
focused module with an LLM-friendly top-level `//!` docstring that explains:
short purpose, `# Contents`, `# Invariants`, and `# See also` when relevant.
Keep `lib.rs` as the crate map, public re-export surface, and small glue only.
When touching an already oversized `lib.rs`, prefer extracting the new work to a
named module instead of adding more large type/function blocks.

Do not expose `Rc`, `RefCell`, raw VM handles, or other single-threaded interior
mutability types across runtime/compiler/package-manager public boundaries.
New boundary DTOs should be owned data (`String`, `Vec`, maps with deterministic
ordering where needed) and remain `Send`/`Sync` friendly. Existing internal
compiler builder state that uses `Rc<RefCell<_>>` is legacy implementation
detail; do not expand it or copy the pattern into runtime/session APIs.

## Agent Checklist (per task)

1. **Confirm intent + constraints**: Web API compatibility? sandbox/permissions? performance target? platform?
2. **Check ES conformance status**: Before working on feature implementation or bug fixes, consult `ES_CONFORMANCE.md` to understand the current pass rate for the affected area.
3. **Search before adding**: prefer `rg` to find similar code and reuse existing patterns.
4. **Keep patches surgical**: avoid refactors unless requested; keep public APIs stable.
5. **Respect safety boundaries**: follow the `unsafe` rules and GC invariants below.
6. **Update the "triangle" when needed**: runtime behavior ↔ TypeScript `.d.ts` ↔ docs/examples/tests.
7. **Parse JS/TS with ASTs**: use `oxc`/SWC; never regex-parse JS/TS.
8. **Protect the runtime boundary**: do not add dependencies from active crates into parked compatibility shims.
9. **Prefer build-graph cleanup**: when a slice lands, check whether temporary shims, adapters, or parked code can be simplified immediately.
10. **Use porting markers for uncertain migrations**: for substantial ports from parked shims or reference implementations, follow `docs/site/src/content/docs/contributing/porting.md` (`TODO(port)`, `PERF(port)`, `PORT NOTE`, optional `PORT STATUS`).

## Repository Map (where to change what)

### Core Runtime Crates
- `crates/otter-gc`: garbage collector.
- `crates/otter-vm`: VM, interpreter, value model, intrinsics, source compiler.
- `crates/otter-runtime`: public runtime and embedding surface.

### Supporting crates
- `crates/otter-bytecode`: bytecode representation and disassembly.
- `crates/otter-syntax` / `crates/otter-compiler`: frontend and lowering.
- `crates/otter-test262`: active ECMAScript conformance runner.
- `crates/otter-cli`: CLI (`otter`).
- Future macro / module / Web API crates must be added under `crates/*`, not under legacy `crates-legacy/*`.

### Parked Compatibility Shims
- `crates-legacy/otter-nodejs`
- `crates-legacy/otter-node-compat`

## File Naming Conventions

### Rust Module Documentation

Every new or materially changed Rust module must keep its top-level
`//!` docstring accurate in the same style as the rest of the active
engine: short purpose, `# Contents`, `# Invariants`, and `# See also`
sections when the module owns non-trivial runtime behavior. When a task
removes a known limitation, update or delete stale "foundation gap",
"task N", and "TODO until GC" wording in the same patch as the code.
Docstrings should describe the current production behavior, not the
history of how the port got there.

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

New ECMAScript builtins, global namespaces, Web API globals, and extension-visible host objects must follow the descriptor/spec/builder/bootstrap flow documented in `docs/site/src/content/docs/extensions/js-surface-builders.md`.

- Add new bootstrap work in `crates/otter-vm` / `crates/otter-runtime`.
- Keep global installation centralized; do not scatter ad-hoc global mutation across unrelated modules.
- Prefer static specs plus mutator-bound builders over one-off registration functions when exposing JS-visible constructors, prototypes, and namespaces.
- High-level APIs must compile down to the same runtime shape as handwritten static specs: no per-call allocation, runtime metadata parsing, or hot-path dynamic registry.
- Contributor-facing workflow docs belong in `docs/site/src/content/docs/`; task files are implementation history.
- If a feature exists only in parked code, port or redesign it; do not grow the parked surface.

## Development Philosophy

- **Production-ready code**: No premature micro-optimizations. Write clean, idiomatic Rust first.
- **Performance target**: High-performance execution with competitive benchmarks.
- **API compatibility**: Prioritize compatibility with web standards.
- **AST-first parsing**: Use ASTs via `oxc`/SWC for JS/TS analysis or transforms; do not use regex to parse JS/TS code.
- **Idiomatic Rust**: Follow Rust best practices, use proper error handling, leverage the type system.
- **Secure defaults**: deny-by-default permissions; new capabilities must be explicit and testable.

## Macro and Async Agreements

### Macro usage

Macros are planned as zero-cost contributor ergonomics over the static spec / builder backend. Do not add macro-first APIs that bypass the builder/bootstrap layer.

Initial macro scope:

- `#[js_class]` for constructor-backed JS classes
- `#[js_namespace]` for namespace-style JS objects
- `raft!` or equivalent grouped static-spec declaration

Deferred until their backend APIs are stable:

- `#[dive]` / async native binding sugar
- `burrow!` for host-owned object surfaces
- `lodge!` for hosted module declarations
- GC trace derive macros

Rules:

- Macros must generate static specs plus normal Rust functions; they are syntax sugar over JS surface builders, not a parallel runtime registry.
- Generated builtins should use the static native function-pointer path by default.
- Keep exported JS names and arity explicit in the macro declaration. Do not hide API shape in unrelated helper code.
- If a macro-based API surface changes, update tests, `.d.ts` declarations, and Astro docs in the same patch when applicable.

Keep code manual when:

- capability enforcement is the main behavior
- bootstrap/install order is delicate
- the macro would hide important control flow

### Async model

- Timers are runtime primitives, not Node-specific APIs:
  - `setTimeout`, `setInterval`, `setImmediate`, `queueMicrotask` belong in the core runtime.
  - `node:timers` / `timers`, if exposed again, must re-export runtime globals rather than grow a separate backend.
- Promise settlement for async native APIs must go through the target VM/runtime job queue.
- Worker tasks may execute plain Rust async work, but VM/JS interaction must hop back onto the runtime scheduling boundary.
- For stream/iterator-like async APIs, use explicit pending queues and deterministic delivery semantics.
- When reviving parked async APIs, move semantics first and adapter glue second; do not preserve old abstractions just for compatibility.

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
**Problem**: `Value`/`JsObject`/`JsString` are raw `Copy` cage offsets; the young
generation is a moving collector, so a value held in a Rust local goes stale
(and is later silently "laundered" into a wrong object) the moment a later
allocation triggers a collection.
**Solution**: Build values inside a handle scope — `ctx.scope(|ctx, s| …)`
with the `scoped_*` methods. Handles live in a collector-traced arena and can
never go stale; the compiler stops them escaping the scope. **This is the
standard API for all native value building** — see
[the handle-scopes doc page](docs/site/src/content/docs/extensions/handle-scopes.md).
Do not add new code with the
deprecated manual `value_roots` threading or raw-`Value` juggling; verify any
multi-allocation native under `OTTER_GC_STRESS=1..16` (identical output every
stride).

### 4. Non-deterministic Test Failures
**Problem**: Tests pass/fail randomly due to hash map iteration order.
**Solution**: Sort keys before comparison, or use ordered collections throughout.

## Build Commands

```bash
# Build
cargo build                          # Debug build
cargo build --release -p otter-cli     # Release CLI binary

# Test
cargo test --all --all-features      # Run all tests

# Lint
cargo fmt --all                      # Format code
cargo clippy --all-targets --all-features -- -D warnings

# Run scripts
cargo run -p otter-cli -- run <file>   # Run a script
cargo run -p otter-cli -- check <file> # Type check with tsgo

# Quick local loop
just fmt && just lint && just test
```

Justfile shortcuts available: `just fmt`, `just lint`, `just test`, `just build`, `just release`

Fast iteration tips:
- Run VM tests: `cargo test -p otter-vm`
- Run runtime tests: `cargo test -p otter-runtime`
- Run a single active support crate after porting work there: `cargo test -p otter-modules`, `cargo test -p otter-web`, etc.

## Architecture

### Crate Hierarchy (bottom to top)

```
otter-cli (CLI -> `otter`)
    ↓
    host/runtime integration layer
    ↓
otter-runtime
    ↓
otter-vm
    ↓
otter-gc
```

Supporting crates should live under `crates/*`. Legacy crates under
`crates-legacy/*` are reference-only and must not be added to the active build
graph.

### Key Architectural Constraints

1. **GC Safety**: Values must be properly rooted when stored across GC boundaries. Use the active GC/reference types and rooting patterns.

2. **Value Representation**: The value model lives in `crates/otter-vm/src/lib.rs`.

3. **Object Model**: The object model lives in `crates/otter-vm/src/object.rs`.

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

Native/builtin contributors on the active stack should allocate and
mutate through the explicit context APIs (`NativeCtx::alloc[_old]`,
`NativeCtx::record_write`, `NativeCtx::reserve_external`, and branded
`GcSession` entry). Use `EscapableHandleScope` when returning one
`Local` out of a nested handle scope. Do not expose or call raw heap
mutation, raw slot visitors, `otter_gc::raw::*`, or manual write
barriers from contributor-facing code.

## Platform Support

Pure Rust implementation - no external JavaScript engine dependencies.

- **macOS**: x86_64, ARM64
- **Linux**: x86_64, ARM64
- **Windows**: x86_64

## Debugging

- Long-running scripts/servers: use `--timeout 0` (disables the timeout).
- Reproducible tier selection: use
  `--jit-tier=production-tiered`, `--jit-tier=template`, or
  `--jit-tier=interpreter`. Legacy JIT environment variables are translated
  only at the CLI boundary.
- When editing embedded JS shims: they are compiled in via `include_str!` and passed through `CString::new(...)` (no `\0` bytes).
- Bytecode disassembly (compile and exit):
  - Text: `cargo run -p otter-cli -- --dump-bytecode <file>`
  - JSON: `cargo run -p otter-cli -- --dump-bytecode=json <file>`
- VM step trace for a script:
  - Stderr: `cargo run -p otter-cli -- --trace=- run <file>`
  - File: `cargo run -p otter-cli -- --trace=otter-trace.txt run <file>`
  - The shipped step trace is versioned text and records
    interpreter-dispatched opcodes only. A `.json` extension does not change
    its format, and native JIT bodies do not currently emit step events.
- Structured JIT events:
  - Default artifact path:
    `cargo run -p otter-cli -- --jit-events --jit-tier=template run <file>`
  - Explicit path:
    `cargo run -p otter-cli -- --jit-events=/tmp/otter-jit-events.json --jit-tier=production-tiered run <file>`
  - `--jit-events=-` writes the JSON report to stderr. Prefer a file when the
    program itself uses stderr or when `--json` is active.
  - The `otterJitDebugSchemaVersion: 1` report contains typed compile,
    inlining, bail, and inline-deopt events. Capture is default-off and bounded
    to 16,384 events per top-level run; `truncated` and `droppedEvents` report
    overflow without constructing further payloads.
  - Abrupt VM completion (for example, a thrown exception after tier-up) still
    writes the partial report. The original execution error remains primary if
    writing that report also fails. A host command timeout can precede isolate
    report delivery; no empty artifact is fabricated in that case.
- JIT compile artifacts:
  - Default directory:
    `cargo run -p otter-cli -- --jit-artifacts --jit-tier=template run <file>`
  - Explicit directory:
    `cargo run -p otter-cli -- --jit-artifacts=/tmp/otter-jit-artifacts --jit-tier=production-tiered run <file>`
  - Combine `--jit-events=<path>` and `--jit-artifacts=<directory>`;
    `codeObjectId` joins a successful compile event to its bundle manifest.
  - The artifact target must not already exist. Under the cooperative
    single-writer contract, the CLI writes a private sibling and atomically
    renames the complete root into view. This is not crash-durable storage or
    a cross-process no-clobber primitive.
  - Each compile directory contains exact runtime-local `code.bin`,
    portable semantic `code-normalized.bin`, typed `relocations.json`,
    annotated ARM64 `asm.txt`, `bytecode.txt`, tier input, `code-map.json`,
    `safepoints.json`, and optimizer `deopt.json` when applicable.
  - Inspect the first line of `optimized-ir.txt` before reading it: the general
    backend emits the Otter optimized unit, while a Cranelift numeric leaf
    starts with `; backend=cranelift numeric-leaf` and then contains CLIF. Its
    code map uses the `craneliftNumericLeaf` structural region.
  - Exact code may contain process addresses and is not a portable golden.
    Compare `code-normalized.bin` across processes; its relocation tokens and
    branch targets are symbolic, and it is not executable. `relocations.json`
    uses exact `code.bin` offsets but never serializes resolved addresses.
  - `asm.txt` starts with `; otter jit aarch64 assembly v1` and
    `; offset-basis=code.bin`. Native locations use `+0x<8-hex>:` offsets,
    local branch targets use `L<8-hex-offset>` labels, and baked address sites
    replace the address-bearing sequence with a symbolic `relocation …` line
    rather than printing its resolved value or immediate chunks.
    Words the decoder does not recognize remain visible through a `.word`
    fallback. Join assembly ranges to bytecode/tier operations through
    `code-map.json`, then inspect `deopt.json` or `safepoints.json` as
    applicable; safepoint `nativeReturnOffset` is currently explicitly `null`.
  - Assembly decoding and formatting run only when `--jit-artifacts` is
    requested. The disabled path does not clone code, disassemble it, or build
    artifact text.
  - Full workflow and schema notes:
    `docs/site/src/content/docs/engine/jit-debugging.md`.
- CPU profile + folded flamegraph stacks:
  - `cargo run -p otter-cli -- run <file> --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Optional: `--cpu-prof-interval 1000 --cpu-prof-name my-run`
  - Produces both `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph.pl).
  - Stack samples are captured by an opt-in bytecode-dispatch sampler from live
    VM frames. Native JIT execution is currently a sampling blind spot.
  - The direct synchronous runtime used by `--cpu-prof` does not enforce
    `--timeout` yet; bound potentially hanging profiler runs externally.
  - Baseline overhead sanity check (`cpu-prof` should stay opt-in): compare `/usr/bin/time -p target/release/otter run <file>` vs `/usr/bin/time -p target/release/otter run --cpu-prof --cpu-prof-dir /tmp/otter-prof <file>` and watch `real` / `sys` delta.
  - Script args are forwarded to `process.argv`: `cargo run -p otter-cli -- run benchmarks/cpu/flamegraph.ts math 2`
  - Shorthand mode also forwards args: `cargo run -p otter-cli -- benchmarks/cpu/flamegraph.ts json 1`
- Test262 timeout triage:
  - `cargo run -p otter-test262 -- run --filter "<pattern>" --timeout 20000`
  - `--timeout` is milliseconds (maximum 30000). Timeouts are recorded in the
    result; failure/timeout trace capture is planned but not yet implemented.
- Embedder-only inspector APIs currently expose IC, shape-transition, frame,
  heap-summary, and Chrome `.heapsnapshot` snapshots. See the docs-site
  [Step Trace](docs/site/src/content/docs/engine/step-trace.md) page.

### Debug Workflows (engine improvement)

- Timeout/hang triage:
  - Bound the run with `--timeout <seconds>`.
  - Capture the current text step trace with `--trace=<path>`.
  - Correlate the final `pc`/opcode with `--dump-bytecode`.
  - Timeout ring-buffer dumps and trace filtering are roadmap items, not
    current CLI flags.
- CPU hotspot triage:
  - Capture profile: `cargo run -p otter-cli -- run benchmarks/cpu/flamegraph.ts <mode> <scale> --timeout 0 --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Inspect `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph).
  - Treat JIT-heavy profiles as incomplete until native code-range
    symbolization lands; compare counters and wall time as supporting evidence.
  - Compare hottest frames before/after optimization patches; do not rely only
    on total runtime.
- Async/host-op tracing is not implemented yet. Do not document planned flags
  as available commands.

### Debug/Profiling Roadmap Rules

- Track all debug/trace/profiling work in `DEBUG_TRACE_PROFILING_PLAN.md`.
- If a patch adds or changes debug/profiling behavior, update:
  1. Runtime behavior (Rust code)
  2. CLI/API surface
  3. `DEBUG_TRACE_PROFILING_PLAN.md` status checkboxes
  4. This `AGENTS.md` section when developer workflow changes
- Keep tooling default-off (minimal overhead unless explicitly enabled).
- Prefer machine-readable outputs (`.trace.json`, `.cpuprofile`,
  `.heapsnapshot`, `.folded`) over ad-hoc text when adding new tooling.
- Output compatibility is mandatory:
  - `*.trace.json` must follow Chrome Trace Event format (`traceEvents`) for DevTools/Perfetto.
  - `*.cpuprofile` must follow Chrome/V8 profile schema for DevTools/Speedscope.
  - `*.heapsnapshot` must follow Chrome heap snapshot schema for DevTools Memory.
  - `*.folded` must use standard folded stack format for flamegraph tools.
- Do not introduce Otter-only primary profiling formats when a standard format exists.

## Security Model

Capability-based, deny-by-default. All permission work belongs in the runtime integration layer:
- `fs_read`, `fs_write` - Path allowlists
- `net` - Host allowlists
- `env` - Variable allowlists with built-in deny patterns for secrets (AWS_*, *_SECRET*, etc.)
- `subprocess`, `ffi` - Boolean flags

Practical rules when adding/altering APIs:
- **Never bypass capabilities**; enforce checks in the Rust boundary and cover with tests.
- **Env access must stay isolated**: preserve default deny behavior and secret deny patterns.

## TypeScript / Types

- Bundled types live in `crates/otter-pm/src/types/` and get installed into `node_modules/@types` for editor resolution.
- `crates/otter-pm/src/types/otter/` is the source of truth for Otter `.d.ts` files.
- `packages/otter-types/` is a publish artifact and should be generated from that source, not edited independently.
- If you add a new global API or built-in module surface, update the corresponding `.d.ts` file(s).
- Type checking integration (tsgo) is still being re-enabled on the current runtime/compiler path.

## CLI Notes

- Default config file search: `otter.toml`, `otter.config.toml`, `.otterrc.toml` (walks up parent dirs).
- Permissions flags are additive/overriding: `--allow-read/--allow-write/--allow-net/--allow-env`, plus `--allow-run` and `--allow-all`.
- Direct run is supported: `cargo run -p otter-cli -- path/to/script.ts` (no `run` subcommand).
- Script argv forwarding is supported in both forms:
  - `cargo run -p otter-cli -- run path/to/script.ts arg1 arg2`
  - `cargo run -p otter-cli -- path/to/script.ts arg1 arg2`

## Benchmarks

- VM tests: `cargo test -p otter-vm`
- Runtime tests: `cargo test -p otter-runtime`
- Test262 conformance: `cargo test -p otter-test262`
- Focused engine harness:
  `cargo run --release -p otter-benchmark --features engine --bin otter-engine-benchmark -- <subcommand>`
  - Current subcommands are `call`, `idle-memory`, `jit-compile`, `memory`,
    and `module`.
  - `call` and `module` require `--jit-tier=interpreter`,
    `--jit-tier=template`, or `--jit-tier=production-tiered`. `jit-compile`
    requires `--compile-tier=template` or `--compile-tier=optimizing` plus
    explicit numeric `--argument` values. `memory` is intrinsically
    interpreter/full-GC; `idle-memory` aggregates fresh release
    process/runtime samples across a controlled post-full-GC idle window. Do
    not infer a benchmark tier from legacy JIT environment variables.
  - `module --runtime-reuse=fresh-per-sample` uses a new runtime for every
    measured execution; `reused-across-samples` reuses one validated runtime.
    Runtime reuse is not a module-cache hit.
  - Every command emits the one live machine-readable result format. Format
    changes are hard breaking: update the runner, fixtures, tests, and
    `benchmarks/README.md` together; do not add compatibility readers or
    legacy output modes.
  - Failed, timed-out, unavailable, and unvalidated observations are
    non-scoreable and must remain visible. A dirty but validated observation
    may remain scoreable for local investigation, but is never
    baseline-eligible.
  - Engine fixtures live under `benchmarks/fixtures/engine/`.
- External suite runners and the baseline capture protocol are documented in
  `benchmarks/README.md`. Raw local results belong under the ignored
  `benchmarks/results/` directory.
- Clean engine baseline capture:
  `cargo run --locked --release -p otter-benchmark --features engine --bin otter-engine-baseline -- capture`
  - The fixed matrix runs serially and keeps its outer watchdog in the
    unversioned capture manifest; successful engine records retain
    `sampling.timeoutMs: null`.
  - Publish only with the same binary's `publish --capture <ignored-dir>`
    command. It revalidates every record and creates the one current
    `benchmarks/baseline/` directory only after measurement is complete.
- Files under `benchmarks/archive/` are historical evidence only. Their old
  binary names, commands, tier labels, and data formats are unsupported and
  must not be presented as the current baseline.

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
