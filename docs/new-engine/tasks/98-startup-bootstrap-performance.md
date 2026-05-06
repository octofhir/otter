# Task 98 — Startup, bootstrap, and first-run performance

## Status

- [x] open after task 96 installs the centralized bootstrap registry
- [x] cold startup benchmark suite added or extended
- [x] bootstrap allocation and install-order telemetry added
- [x] lazy/tiered builtin installation evaluated
- [x] optional startup snapshot / code-cache plan prototyped or explicitly rejected
- [x] startup budgets documented in mdBook
- [x] gates green

## 2026-05-06 implementation notes

Task 98 added focused active-stack Criterion ratchets:

- `crates-next/otter-vm/benches/bootstrap.rs`
  - `bootstrap_global_this/default_features`
  - `bootstrap_global_this/core_without_console`
  - `bootstrap_global_this/default_features_with_telemetry`
- `crates-next/otter-runtime/benches/startup.rs`
  - `runtime_builder_build/default`
  - `runtime_builder_build/production_sandbox`
  - `runtime_builder_build/otter_builder_default`
  - `runtime_first_run/javascript_undefined`
  - `runtime_first_run/typescript_undefined`
  - `runtime_first_run/static_native_extracted_math_abs`
- `crates-next/otter-cli/benches/cold_start.rs`
  - `cli_cold_start/eval_empty`
  - `cli_cold_start/tiny_js_file`
  - `cli_cold_start/tiny_ts_file`

Task 98 initially capped actual runtime constructions to 32 per Criterion
sample to avoid exhausting the process-global GC cage. Task 99 / 91.8
removed that workaround: dropping the final public runtime handle now joins
the isolate runner, the GC exposes cage occupancy diagnostics, and the
startup bench immediately drops each runtime after measuring the
build/first-run body.

Bootstrap telemetry is default-off. Production `Interpreter::new()` still
calls the plain bootstrap path. Benches and focused tests can call
`bootstrap::build_global_this_with_telemetry` to collect:

- registry entries considered/installed/skipped;
- installed objects, prototype objects, namespace objects, and native
  functions;
- string interning count;
- GC allocation count and live-byte delta;
- duplicate-name validation count/result;
- per-entry phase timings.

Current telemetry ratchet:

- all default entries install;
- duplicate registry names: `0`;
- string interning during bootstrap: `0`;
- namespace objects installed: `4` (`JSON`, `Math`, `Atomics`, `console`);
- static native functions installed through specs: `57`;
- GC allocation delta must stay `<= 160`;
- GC live-byte delta must stay `<= 96 KiB`.

The handwritten-vs-macro startup comparison remains the Task 97
`js_surface_macros` benchmark report. Macro-generated namespaces/classes
feed the same static specs and `NativeCall::Static` builder path as
handwritten specs, so Task 98's startup coverage protects both generated
and handwritten surfaces through the shared builder/bootstrap path.

## 2026-05-06 local benchmark results

Machine/date: local macOS development machine, 2026-05-06.
Profile: Cargo `bench` / optimized Criterion binaries.

Commands:

```bash
cargo bench -p otter-vm --bench bootstrap -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-runtime --bench startup -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-cli --bench cold_start -- --sample-size 10 --measurement-time 2 --warm-up-time 1
```

Results:

| Benchmark | Median-ish Criterion interval |
|---|---:|
| `bootstrap_global_this/default_features` | 112.81-114.53 us |
| `bootstrap_global_this/core_without_console` | 110.77-113.61 us |
| `bootstrap_global_this/default_features_with_telemetry` | 117.79-125.46 us |
| `runtime_builder_build/default` | 115.77-125.42 us |
| `runtime_builder_build/production_sandbox` | 117.50-125.79 us |
| `runtime_builder_build/otter_builder_default` | 402.59-443.12 us |
| `runtime_first_run/javascript_undefined` | 120.98-130.90 us |
| `runtime_first_run/typescript_undefined` | 116.69-127.48 us |
| `runtime_first_run/static_native_extracted_math_abs` | 116.92-127.06 us |
| `cli_cold_start/eval_empty` | 25.142-25.548 ms |
| `cli_cold_start/tiny_js_file` | 25.304-25.580 ms |
| `cli_cold_start/tiny_ts_file` | 25.350-26.343 ms |

The `Otter::builder().build()` number includes starting the sendable
runtime handle/isolate runner. The `RuntimeBuilder::build()` numbers cover
the local isolate layer only.

## 2026-05-06 closeout gates

Completed:

```bash
cargo fmt --all
cargo test -p otter-vm -p otter-runtime
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
mdbook build docs/book
cargo bench -p otter-vm --bench bootstrap -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-runtime --bench startup -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-cli --bench cold_start -- --sample-size 10 --measurement-time 2 --warm-up-time 1
```

Static fff checks:

- no product-code `GcHeap::with_thread_default` / `enter_thread_default`
  hits;
- no hot-path `HashMap<String, Box<dyn Fn...>>` registry;
- async hits are existing runtime-owned futures or compile-fail fixtures,
  not new Task 98 code storing builder/context/`Value`/`Gc`/`Local` across
  futures.

## Lazy/tiered install evaluation

The current registry already has coarse feature tiers:

- `CORE` for language globals and standard builtins;
- `CONSOLE` for the host console surface.

The Task 98 bench includes `core_without_console`, which shows the feature
gate works and avoids installing console methods when disabled. Further
lazy initialization of currently-listed standard globals is rejected for
this task: lazily installing `Math`, `JSON`, `Atomics`, or placeholder
constructor-shaped globals would make `globalThis` property enumeration,
identity, and first-access timing observably different from eager
bootstrap. Larger optional families (`Intl`, `Temporal`, Web APIs, hosted
modules) should remain feature-tiered as their active-stack install
surfaces land, but Task 98 does not introduce proxy/lazy mutation.

## Snapshot/code-cache decision

Startup snapshot and bootstrap code-cache are explicitly rejected for Task
98. The current moving, isolate-local GC heap still needs a versioned root
serialization story, pointer/cage invalidation rules, and a policy for
embedder memory footprint before a serialized heap can be production-safe.
The measured local startup numbers are low enough that ratchets and
telemetry give better value than a snapshot format now. Revisit only if
CLI cold start or embedder startup budgets regress after larger Web API or
module surfaces land.

## Goal

Make engine initialization fast enough for production CLI and embedding
use cases. A convenient high-level API is not sufficient if every runtime
construction eagerly allocates and wires every possible builtin surface.

Breaking changes are allowed. Prefer a smaller, faster, deterministic
startup path over preserving interim bootstrap APIs.

## Source

- [`96-production-js-surface-builders.md`](./96-production-js-surface-builders.md)
  centralized bootstrap registry and static specs.
- [`91-gc-bench-and-soak-infra.md`](./91-gc-bench-and-soak-infra.md)
  benchmark infrastructure.
- Roadmap startup/cache direction, including baseline-only startup module
  cache work.

## Scope

### 98.1 — Startup benchmark targets

Add stable benchmark targets for:

- `RuntimeBuilder::build()` with default config;
- `RuntimeBuilder::build()` with common production config;
- first `run_script("undefined;")`;
- first TypeScript run if TS lowering remains in-process;
- CLI cold `otter -e ""` or equivalent command path;
- CLI cold run of a tiny `.js` file;
- CLI cold run of a tiny `.ts` file.

Benchmarks must report wall time and allocation counts where practical.

### 98.2 — Bootstrap telemetry

Add opt-in debug/bench telemetry for bootstrap:

- number of objects/functions/prototypes installed;
- number of strings interned;
- number of GC allocations and bytes;
- per-bootstrap-phase timing;
- duplicate-name / install-order validation cost.

Telemetry must be default-off in production runtime paths.

### 98.3 — Tiered and lazy initialization

Evaluate splitting bootstrap into tiers:

- minimal language globals required for script execution;
- standard ES builtins;
- ECMA-402 / Temporal / larger optional families;
- Web APIs;
- hosted modules / Node compatibility surfaces.

Lazy initialization must preserve spec-observable behavior. If lazy install
would change observable property enumeration or identity semantics, reject
it for that surface and document why.

### 98.4 — Startup snapshot / code cache decision

Prototype or explicitly reject a startup snapshot / baseline code-cache
path for bootstrap modules and common builtins. The decision must include:

- compatibility with the GC heap and roots;
- invalidation/versioning story;
- impact on CLI cold start;
- impact on embedder memory footprint;
- maintenance cost.

Do not introduce a custom primary profiling format. Use existing benchmark
and trace outputs.

### 98.5 — Production budgets

Document startup budgets in mdBook and task closeout notes. Exact numbers
may evolve, but the task must create a ratchet so future bootstrap/API work
cannot regress startup silently.

## Out of scope

- JIT performance.
- Full Node compatibility startup optimization.
- Shipping a stable serialized heap snapshot format unless the prototype
  proves it is worth the maintenance cost.

## Validation gates

- [x] Startup benchmarks run locally and in the selected CI/perf workflow.
- [x] Before/after report for task 96/97 surfaces is recorded.
- [x] No new always-on telemetry in production hot paths.
- [x] Lazy/tiered install decisions are documented with spec-observable
  behavior notes.
- [x] mdBook documents how contributors run startup benchmarks and what
  budget they must protect.

## Closing

Update the task index, roadmap startup notes if needed, and mdBook
contributor/performance docs.

Closed 2026-05-06. Follow-up for unbounded cold-start bench memory
isolation lives in Task 91.8.
