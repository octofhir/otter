# GC Migration Baseline — pre-Phase 1 snapshot

Captured: 2026-04-26 on commit `cd49356`.

## Build status

| Command | Result |
|---|---|
| `cargo check -p otter-gc -p otter-vm -p otter-runtime` | clean, 7.47 s |
| `cargo test -p otter-gc --lib --no-run` | clean |
| `cargo test -p otter-vm --lib --no-run` | clean |
| `cargo test -p otter-runtime --lib --no-run` | clean |

## Lib test counts (debug build)

| Crate | Tests | Source |
|---|---|---|
| `otter-vm` | 1044 | `target/debug/deps/otter_vm-*` |
| `otter-runtime` | 7 | `target/debug/deps/otter_runtime-*` |
| `otter-gc` | 86 | `target/debug/deps/otter_gc-*` |
| **Total** | **1137** | — |

These are the floor counts. The migration must keep all 1137 tests
passing through every phase. Net additions are welcome; net regressions
block the phase.

## Architecture as of baseline

`crates/otter-vm/src/object.rs::ObjectHeap` wraps
`crates/otter-gc/src/typed.rs::TypedHeap`. `TypedHeap` is the only
allocation backend in use.

- Storage: `slots: Vec<Option<Slot>>` where `Slot = { Box<dyn
  TypeErasedObject>, size: usize, is_young: bool, survived: bool }`.
- Allocation cost: `Vec::push` + `Box::new` ≈ 50 cycles + alloc syscall.
- Collection: STW mark-sweep; mark = `Vec<bool>` parallel to slots,
  sweep iterates the entire slot table.
- `is_young`/`survived` flags exist but are `#[allow(dead_code)]`;
  there is no actual generational behaviour.
- No write barriers. No incremental marking. No concurrent marking.
- Pause @ 1 GB heap: estimated 10–100 ms per `PRODUCTION_READINESS_PLAN.md`
  §3.2.

The page-based GC scaffolding in `crates/otter-gc/src/{heap,scavenger,
marking,barrier,page,space,trace,handle,header}.rs` is fully
implemented but has zero call sites from `otter-vm`. See
`docs/gc-migration-plan.md` for details.

## Performance baseline (deferred)

A heavyweight benchmark suite is not yet run because the migration
plan is staged across phases that themselves need before/after
numbers. Microbenchmarks for:

- alloc throughput (`gc_alloc_throughput`)
- P50 / P99 GC pause @ 1 GB heap, sustained 500 MB/s alloc rate
  (`gc_pause_p99`)

…will be added under `crates/otter-gc/benches/` as part of Phase 6
acceptance. The hyperfine numbers for the existing
`crates/otter-vm/benches/c2_string_bench.rs` are unaffected by GC
work and stay the responsibility of the C2 perf track.

## Test262 baseline (deferred per request)

The user has standing instructions not to run full test262 sweeps
unless explicitly asked. Per-phase test262 spot checks are
`bash scripts/test262-safe.sh built-ins/Array`, recorded in each
phase acceptance line.
