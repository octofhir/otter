# Task 91 — GC benchmark + soak-test + sanitizer CI infrastructure

## Status

- [ ] Criterion bench harness in `crates-next/otter-gc/benches/`
- [ ] `cargo fuzz` corpus + targets in `crates-next/otter-gc/fuzz/`
- [ ] 24 h soak-test runner script
- [ ] miri / asan / lsan / tsan CI jobs
- [ ] V8-parity benchmark suite
- [ ] allocator/backing-store benchmark matrix inspired by Oscars'
      workload comparisons
- [ ] runtime-handle leak / stress diagnostics integrated with GC soak
- [x] cold-start benchmark memory isolation / GC cage teardown plan
- [ ] gates green

## Why this exists

Phase 1 closeout (task 84) and Phase 2 closeout (task 86) carry
production-grade gates that **measure** the GC: pause histograms,
throughput parity vs. V8, soak-test endurance, sanitizer
cleanliness, fuzz corpus survival. ADR-0005 also requires early
detection for public async handles: leaked host ops, timers, command
waiters, global handles, and stress-GC around async completion. These
gates need infrastructure to run against. Without this task, the gates
cannot be checked.

This task is the foundation of "production-grade" as a verifiable
claim, not a self-assessment.

**Slot in the plan:** runs in parallel with the migration tasks
(76–83). Some pieces (Criterion benches) come up early so each
migration can capture its perf delta; some pieces (soak runner,
24 h CI job) only need to be ready by task 84.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.2
(NF1, NF2, NF7, NF8, NF10), §7 (diagnostic surface).
Research input: Boa Oscars' allocator experiments are useful as a
benchmark taxonomy, not as a backend swap. Copy the workload categories
(allocation pressure, mixed graph, vector/backing-store churn, memory
pressure, weak-map churn) and measure them against Otter's page heap,
Node/V8, and previous Otter baselines.

## Scope

### 91.1 — Criterion bench harness

2026-05-06 progress: active `otter-gc` now has eight safe Criterion targets
checked into `crates-next/otter-gc/benches/`:

- `bench_alloc_young_bump.rs`;
- `bench_alloc_with_barrier.rs`;
- `bench_scavenge_4mb.rs`;
- `bench_collect_full_256mb.rs`;
- `bench_card_table_dirty_scan.rs`;
- `bench_handle_scope_overhead.rs`;
- `bench_pointer_compress_decompress.rs`;
- `bench_backing_store_accounting.rs`.

The remaining `collect_full_1gb` and `weakmap_churn` targets stay open.
Do not add a default 1 GiB bench run until the workflow has an explicit
memory policy; user-facing local validation should not unexpectedly consume
gigabytes.

Files:
```
crates-next/otter-gc/benches/bench_alloc_young_bump.rs
crates-next/otter-gc/benches/bench_alloc_with_barrier.rs
crates-next/otter-gc/benches/bench_scavenge_4mb.rs
crates-next/otter-gc/benches/bench_collect_full_256mb.rs
crates-next/otter-gc/benches/bench_collect_full_1gb.rs
crates-next/otter-gc/benches/bench_card_table_dirty_scan.rs
crates-next/otter-gc/benches/bench_handle_scope_overhead.rs
crates-next/otter-gc/benches/bench_pointer_compress_decompress.rs
crates-next/otter-gc/benches/bench_backing_store_accounting.rs
crates-next/otter-gc/benches/bench_weakmap_churn.rs
```

Each bench produces a stable number; results are checked into
`docs/new-engine/test262-baseline/gc-bench-baseline.md` and
regression-gated in CI.

Smoke results for the lightweight targets:

```text
commands:
cargo bench -p otter-gc --bench bench_pointer_compress_decompress -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench -p otter-gc --bench bench_handle_scope_overhead -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench -p otter-gc --bench bench_card_table_dirty_scan -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench -p otter-gc --bench bench_backing_store_accounting -- --sample-size 10 --measurement-time 1 --warm-up-time 1

pointer_compress_decompress/offset                 349.33-351.47 ps
pointer_compress_decompress/header_ptr             809.80-816.24 ps
handle_scope_overhead/scope_with_16_locals         26.848-27.055 ns
card_table_dirty_scan/one_page_sparse_dirty        438.32-441.36 ns
backing_store_accounting/reserve_release_4k        8.5399-8.7049 ns
backing_store_accounting/resize_4k_to_8k_to_1k     19.337-19.389 ns
```

### 91.2 — `cargo fuzz` corpus

```
crates-next/otter-gc/fuzz/Cargo.toml
crates-next/otter-gc/fuzz/fuzz_targets/fuzz_alloc_collect_cycle.rs
crates-next/otter-gc/fuzz/fuzz_targets/fuzz_handle_scope_nesting.rs
crates-next/otter-gc/fuzz/fuzz_targets/fuzz_weakmap_eviction_pattern.rs
crates-next/otter-gc/fuzz/fuzz_targets/fuzz_pointer_compression_roundtrip.rs
crates-next/otter-gc/fuzz/fuzz_targets/fuzz_card_table_barrier.rs
```

CI nightly: each target ≥ 10 M iterations, no panic, no leak (with
LeakSanitizer wrapper).

### 91.3 — 24 h soak-test runner

`crates-next/otter-test262/src/soak.rs` + a `cargo run -p
otter-test262 -- soak --duration 24h` mode that:
- Runs the full test262 corpus in a tight loop.
- Tracks RSS every 60 s.
- Asserts: zero panics, zero OOM kills, RSS drift ≤ 10 % from cycle 1.
- On exit, dumps a Markdown report into
  `docs/new-engine/test262-baseline/soak-YYYYMMDD.md`.

### 91.4 — Sanitizer + miri CI matrix

GitHub Actions workflow `.github/workflows/gc-sanitizers.yml`:
- `cargo +nightly miri test -p otter-gc -p otter-vm` — every PR.
- `RUSTFLAGS="-Z sanitizer=address" cargo +nightly test` — every PR
  on `crates-next/otter-gc`.
- `RUSTFLAGS="-Z sanitizer=leak" cargo +nightly test` — every PR.
- `RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test --features
  concurrent-sweep` — Phase 2 onward; nightly schedule.
- `cargo fuzz run …` — nightly schedule, per-target 1 h budget.

### 91.5 — V8-parity benchmark suite

`crates-next/otter-test/benches/v8-parity/` — a curated suite of
JS programs run **both** under Otter and under Node.js LTS:
- Object-literal bursts (1 M `{x: 1, y: 2}` allocations).
- Closure chain (1 M nested closures, captured upvalues).
- JSON parse (1 GB JSON document).
- Async/await pipeline (10 K parallel awaits).
- RuntimeHandle stress (many concurrent `Otter::run_*` calls through
  Tokio multi-thread executor).
- Property-store loop (steady-state pointer writes triggering
  barriers).

Runner reports Otter time / Node time ratio. Gates per
architecture doc §1.2 NF10.

### 91.6 — Oscars-style allocator / backing-store matrix

Add workload benches that answer allocator questions before changing
the allocator:

- **vector/backing-store churn:** grow/shrink arrays, strings, typed
  arrays when available, and map/set buckets; verify exact
  `reserve_bytes` / `release_bytes` accounting tracks RSS-relevant
  capacity, not only GC headers.
- **mixed object graph:** allocate objects with nested arrays/maps and
  measure total time, GC time, promoted bytes, and peak RSS.
- **memory pressure:** run under low `max_heap_bytes`; assert catchable
  OOM, emergency full-GC recovery, and no process abort.
- **deep graph:** long prototype/closure/list chains to catch recursive
  trace stack overflow and retained-size walker blowups.
- **weak-map churn:** repeated set/replace/delete/drop-key/drop-map
  cycles to catch stale ephemeron metadata and weak registry leaks.
- **old-to-young mutation storm:** steady-state old object stores young
  values; validates card-table dirty-card scanning and barrier cost.

Do **not** adopt Oscars' `Collector: Allocator` direction for Otter's
object heap. If the numbers show allocator pressure, prefer
heap-accounted backing-store arenas / size classes for off-object
buffers while preserving the moving young generation.

### 91.7 — Runtime-handle leak and stress diagnostics

Add reusable test utilities for task 85 and later server surfaces:

- forced GC at every allocation / safepoint / async completion;
- runtime-drop leak reports for global handles, open handle scopes,
  pending host ops, live timers, queued commands, and unresolved
  host-created promises;
- bounded queue pressure tests;
- cancellation/timeout soak tests where waiters are dropped while the
  isolate continues to a safepoint.

### 91.8 — Cold-start benchmark memory isolation

Task 98 exposed a benchmark-infrastructure gap: in-process Criterion
cold-start loops can construct thousands of fresh runtimes in one long-lived
bench process. Each fresh runtime allocates from the process-wide GC cage,
and today dropping the runtime does not return those pages to the cage. That
can exhaust the cage before the benchmark finishes even when the per-runtime
startup footprint is small.

This is a benchmark/GC lifecycle task, not a reason to remove startup
ratchets.

Implemented 2026-05-06:

- `RuntimeHandleInner::drop` now joins the isolate runner when the final
  public handle is dropped, so the isolate-owned `Runtime` and its `GcHeap`
  are torn down before the next cold-start iteration;
- `otter_gc::cage_stats()` exposes process-global cage occupancy diagnostics
  for tests and benchmark lifecycle checks;
- `crates-next/otter-runtime/tests/runtime_cage_reuse.rs` checks that
  repeated `Otter::builder().build()` / drop cycles return allocated cage
  pages to the pre-build count;
- `crates-next/otter-runtime/benches/startup.rs` no longer caps actual
  runtime constructions per sample. It measures the build/first-run body,
  then drops the runtime immediately in the same iteration so Criterion does
  not retain a batch of live heaps.

Choose and implement one production-grade path:

- make `GcHeap` teardown return owned pages/cage reservations so repeated
  in-process runtime construction is memory-neutral; or
- provide a benchmark runner that isolates cold-start iterations in
  subprocesses and records wall time/RSS per process; or
- both, if embedders also need repeated create/drop isolate churn in one
  process.

Required output:

- a checked-in cold-start memory regression test or bench that records peak
  RSS/cage usage;
- documentation for the safe sample/iteration policy;
- removal or justification of any artificial iteration cap in Task 98
  startup benches.

## Validation gates

- [ ] All eight Criterion benches in 91.1 produce stable numbers
  (variance < 5 % across 5 consecutive runs).
- [ ] All five fuzz targets in 91.2 survive 10 M iterations with
  no panic / no leak.
- [ ] 91.3 soak runner: 24 h run completes; report shows ≤ 10 %
  RSS drift, zero panics.
- [ ] 91.4 CI workflow merged and green.
- [ ] 91.5 V8-parity numbers checked into baseline.
- [ ] 91.6 allocator/backing-store matrix has checked-in baseline
  results and documents whether any allocator change is justified.
- [ ] 91.7 stress utilities are used by task 85 validation tests.
- [x] 91.8 proves repeated cold-start benchmarking cannot exhaust the
  process-wide GC cage, or documents subprocess isolation as the supported
  workflow.

## Closing

Slice 91.1–91.5 into separate sub-task files when starting work.
This is the master scaffold.
