# Task 98 — Startup, bootstrap, and first-run performance

## Status

- [ ] open after task 96 installs the centralized bootstrap registry
- [ ] cold startup benchmark suite added or extended
- [ ] bootstrap allocation and install-order telemetry added
- [ ] lazy/tiered builtin installation evaluated
- [ ] optional startup snapshot / code-cache plan prototyped or explicitly rejected
- [ ] startup budgets documented in mdBook
- [ ] gates green

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

- [ ] Startup benchmarks run locally and in the selected CI/perf workflow.
- [ ] Before/after report for task 96/97 surfaces is recorded.
- [ ] No new always-on telemetry in production hot paths.
- [ ] Lazy/tiered install decisions are documented with spec-observable
  behavior notes.
- [ ] mdBook documents how contributors run startup benchmarks and what
  budget they must protect.

## Closing

Update the task index, roadmap startup notes if needed, and mdBook
contributor/performance docs.
