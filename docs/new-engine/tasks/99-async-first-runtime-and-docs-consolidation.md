# Task 99 — Async-first runtime, CLI startup, and docs consolidation

## Status

- [x] async-first runtime contract defined in mdBook
- [x] public CLI and embedder paths use one async-capable runtime stack
- [ ] sync-only runtime layer removed or reduced to a blocking facade over the
      async runtime
- [ ] CLI cold-start profile captured and optimized below Task 98 baseline
- [x] repeated cold-start benchmark memory isolation fixed
- [ ] obsolete compatibility/runtime/docs paths removed from the active workflow
- [ ] `docs/` cleaned so mdBook is the contributor-facing documentation source
- [ ] gates green

## Goal

Fix the architectural misses exposed by Task 98 in one consolidation slice:

1. Runtime must be async-first. Synchronous convenience APIs are allowed only
   as blocking wrappers over the same async-capable runtime. There should not
   be a separate "sync runtime" strategy that bypasses timers, host ops,
   workers, module loading, or future async Web APIs.
2. CLI cold start must get materially faster than the Task 98 ~25 ms baseline
   without giving up async capability.
3. Startup benchmark infrastructure must stop relying on artificial iteration
   caps caused by process-wide GC cage exhaustion.
4. Documentation must stop being split between mdBook and old task/history
   files. mdBook becomes the only contributor-facing docs tree under `docs/`;
   task/history material is either migrated into mdBook where still useful or
   deleted.

## Baseline From Task 98

Task 98 measured:

- local `RuntimeBuilder::build()`: ~121 us;
- public `Otter::builder().build()`: ~422 us, including the async handle /
  isolate runner;
- first `run_script("undefined;")`: ~126 us;
- CLI `otter -e ""` / tiny `.js` / tiny `.ts`: ~25-26 ms.

Conclusion: core runtime startup is not the 25 ms problem. The remaining
cost is likely process startup, binary/dependency initialization, CLI parser
work, frontend cold initialization, file/module routing, and avoidable runtime
facade layering.

## 2026-05-06 progress notes

Async-first runtime contract is now documented in:

- `docs/book/src/engine/architecture.md`;
- `docs/book/src/engine/event-loop.md`.

CLI execution now starts from `#[tokio::main]` and routes file execution plus
top-level `-e` / `-p` through async `Otter::run_file` / `Otter::eval`.
`RuntimeHandle::block_on` remains crate-private and is used only by the
public `Otter::blocking_*` sync-caller convenience wrappers. The CLI
entrypoint is async by default; embedding can keep a separate public
entrypoint shape as long as it converges on the same async-capable runtime
semantics. Compile-only commands (`check`, `--dump-bytecode`) may still use
local `Runtime` because they do not execute JavaScript and do not need
event-loop behavior.

Rejected approaches:

- a handwritten pre-`clap` fast path for `otter -e`,
`otter -p`, and `otter <file>`. It duplicated parser semantics, would become
  fragile as flags grow, and locally regressed `eval_empty`;
- disabling `clap` default `color` / `suggestions`. Those are useful CLI UX
  features and are not an acceptable tradeoff for small cold-start movement.

Research note: upstream `clap` docs expose derive raw command attributes,
so parser behavior should be optimized through `clap` configuration and
feature flags rather than a second parser. The `Command` API includes knobs
such as `disable_help_subcommand`, `disable_colored_help`, `color(...)`, and
terminal-width settings where the relevant features are enabled. Do not fork
CLI parsing unless profiling proves `clap` is the dominant cost and the UX
tradeoff is explicitly accepted.

Bench after async `main` plus direct `Otter::run_file` / `Otter::eval`
execution path:

```text
command:
cargo bench -p otter-cli --bench cold_start -- --sample-size 10 --measurement-time 2 --warm-up-time 1

cli_cold_start/eval_empty    25.640-26.419 ms
cli_cold_start/tiny_js_file  25.978-26.286 ms
cli_cold_start/tiny_ts_file  25.568-26.322 ms
```

This is not yet below the Task 98 baseline, so the next optimization must
profile process/binary/frontend costs rather than add parser shortcuts.

Cold-start benchmark memory isolation is now fixed:

- dropping the final `RuntimeHandle` joins the isolate runner, so the
  isolate-owned `Runtime`/`GcHeap` releases pages before the next
  create/drop iteration;
- `otter_gc::cage_stats()` records process-global cage occupancy;
- `cargo test -p otter-runtime repeated_otter_build_drop_returns_gc_pages_to_cage`
  proves repeated public `Otter` builds return allocated cage pages to the
  pre-build count;
- `crates-next/otter-runtime/benches/startup.rs` no longer has the Task 98
  per-sample construction cap.

Runtime startup smoke after removing the cap:

```text
command:
cargo bench -p otter-runtime --bench startup -- --sample-size 10 --measurement-time 2 --warm-up-time 1

runtime_builder_build/default                 20.220-20.675 us
runtime_builder_build/production_sandbox      20.139-20.380 us
runtime_builder_build/otter_builder_default   264.21-267.76 us
runtime_first_run/javascript_undefined        22.087-22.347 us
runtime_first_run/typescript_undefined        22.595-23.031 us
runtime_first_run/static_native_math_abs      24.773-27.044 us
```

These runtime-startup numbers measure the build/first-run body and exclude
teardown; teardown still runs immediately after every measured iteration to
keep cage usage bounded.

GC benchmark infra follow-up started under Task 91.1. Four lightweight
active-stack benches were added for pointer decompression, handle-scope
rooting, card-table dirty scanning, and external/backing-store accounting.
The 1 GiB full-GC bench remains intentionally open until the local workflow
has an explicit memory policy; default validation should not unexpectedly
allocate gigabytes.

## Scope

### 99.1 — Define The Async-First Runtime Contract

Document and enforce:

- there is one runtime stack with async/event-loop support;
- sync APIs are blocking adapters over that stack, not an alternate engine;
- CLI one-shot execution still initializes the async-capable runtime boundary;
- native async work never stores `RuntimeCx`, `NativeCtx`, `Value`, `Gc`, or
  `Local` in futures;
- no active-stack product code reaches for thread-local GC heap state.

If any API currently implies a separate sync runtime, rename or remove it.

### 99.2 — Consolidate CLI Execution Paths

Profile and simplify:

- `otter -e ""`;
- `otter -p "1"`;
- `otter run /tmp/tiny.js`;
- `otter run /tmp/tiny.ts`;
- shorthand `otter /tmp/tiny.js`.

Keep async support available from process start, but remove avoidable
double-layering:

- no duplicate runtime construction;
- no duplicate source reads;
- no duplicate parse/check/run path;
- no string/regex parsing of JS/TS module syntax;
- no legacy `crates/*` involvement.

Acceptable outcome: sync-looking CLI commands block on the async-capable
runtime. Unacceptable outcome: a separate sync-only CLI engine path.

### 99.3 — CLI Cold-Start Profiling And Optimization

Break the ~25 ms baseline into buckets:

- process/dynamic-loader startup;
- binary size and linked dependency initialization;
- clap/parser overhead;
- permission/config construction;
- Tokio/event-loop/runtime-handle startup;
- source read and source-kind detection;
- module graph routing;
- OXC parse/compile cold initialization.

Optimize only after the bucket is measured. Candidate areas:

- reduce CLI parser work on hot paths;
- avoid module-routing work for known script/eval inputs;
- defer optional-heavy surfaces without changing observable semantics;
- reduce public facade overhead while preserving async-first behavior;
- audit large optional dependency linkage on CLI cold paths.

### 99.4 — Fix Startup Benchmark Memory Isolation

Task 98 had to cap actual in-process runtime constructions to avoid
exhausting the process-wide GC cage. This task must remove that workaround or
make the isolation policy explicit and permanent.

Choose one:

- `GcHeap` teardown returns pages/cage reservations so repeated create/drop
  runtime churn is memory-neutral; or
- cold-start benchmarks run each measured iteration in a subprocess and record
  wall time plus RSS/cage usage; or
- both.

After this lands, update or remove the Task 98 iteration cap in
`crates-next/otter-runtime/benches/startup.rs`.

### 99.5 — Remove Old Runtime/Compatibility Paths

Audit for active workflow references to:

- parked `crates/*` compatibility shims;
- old runtime names or sync-only runtime wording;
- docs that tell contributors to use legacy stack paths;
- stale task-96/97/98 history that belongs in mdBook only.

Rules:

- legacy crates may remain on disk only as explicitly marked reference code;
- nothing under active `crates-next/*` may depend on legacy crates;
- do not delete a reference file until its still-useful content has either
  moved into mdBook or been declared obsolete in the task closeout.

### 99.6 — Clean `docs/` Around mdBook

End-state target:

```text
docs/
  book/
    book.toml
    src/
```

Required migration before deletion:

- active architecture guidance moves to `docs/book/src/engine/*`;
- contributor workflows move to `docs/book/src/contributing/*`;
- performance/startup/benchmark workflows move to
  `docs/book/src/performance/*`;
- task tracker closeout history that still matters is summarized in mdBook or
  exported to the external issue tracker;
- generated `docs/book/book/` is either kept out of source control or
  documented as generated output.

Only after that migration, delete obsolete `docs/new-engine/*` task/history
files so the docs directory no longer mixes living docs with implementation
history.

## Out Of Scope

- JIT work.
- Startup heap snapshot / serialized code cache unless profiling proves it is
  the only credible path.
- Full Node compatibility startup optimization.
- Any dependency from active crates into legacy `crates/*`.

## Validation Gates

- [ ] `cargo fmt --all`
- [ ] `cargo test -p otter-cli -p otter-runtime -p otter-vm`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `mdbook build docs/book`
- [ ] `cargo bench -p otter-cli --bench cold_start -- --sample-size 10 --measurement-time 2 --warm-up-time 1`
- [ ] `cargo bench -p otter-runtime --bench startup -- --sample-size 30 --measurement-time 2 --warm-up-time 1`
- [ ] static fff checks:
  - no active-stack dependency on `crates/*`;
  - no product-code `GcHeap::with_thread_default` / `enter_thread_default`;
  - no sync-only runtime path bypassing async/event-loop support;
  - no `RuntimeCx` / `NativeCtx` / `Value` / `Gc` / `Local` stored in futures;
  - docs outside mdBook are gone or explicitly generated artifacts.

## Closing

Close only when:

- CLI cold start is materially below the Task 98 baseline or the remaining
  process-startup floor is measured and documented;
- async-first runtime semantics are the only active runtime semantics;
- benchmark memory isolation no longer depends on hidden cage exhaustion
  behavior;
- `docs/` has been migrated to mdBook-only contributor docs;
- old task/history docs and obsolete runtime guidance no longer distract from
  the active stack.
