# Task 74 — GC stats, heap snapshot, retained-size walker

## Status

- [ ] `GcStats` per-type counters
- [ ] `HeapSnapshot` struct
- [ ] retained-size walker
- [ ] `Runtime::heap_stats()` / `Runtime::heap_snapshot()` API
- [ ] gates green

## Goal

Make leaks **observable** before they're reported as host OOM. Tests
must be able to allocate, drop, force `gc.collect_full()`, and assert
"the per-type live byte count returned to baseline". Without this
surface the migration tasks (76+) cannot prove they actually fixed
the cycle they claim to.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §7
("Leak diagnosis"), §1.2 NF6.

## Scope

1. **`GcStats` in `crates-next/otter-gc/src/stats.rs`:**
   ```rust
   pub struct GcStats {
       pub live_objects: usize,
       pub live_bytes: usize,
       pub by_type: [TypeStats; 256],
       pub last_gc_pause_ms: f32,
       pub last_gc_reclaimed_bytes: usize,
       pub gc_cycles: u64,
   }
   pub struct TypeStats {
       pub live_bytes: usize,
       pub alloc_count_total: u64,
       pub free_count_total: u64,
   }
   impl GcHeap { pub fn stats(&self) -> &GcStats; }
   ```
2. **`HeapSnapshot` in `snapshot.rs`:**
   ```rust
   pub struct HeapSnapshot {
       pub objects: Vec<SnapshotObject>,   // (type_tag, retained_size, slot_idx)
       pub roots: Vec<GcRaw>,
       pub edges: Vec<(GcRaw, GcRaw)>,     // parent → child
   }
   impl GcHeap { pub fn snapshot(&self, roots: &[GcRaw]) -> HeapSnapshot; }
   impl HeapSnapshot {
       pub fn retained_size(&self, gc: GcRaw) -> usize;
       pub fn group_by_type(&self) -> [usize; 256];   // total retained by type
   }
   ```
3. **Runtime API in `crates-next/otter-runtime/src/lib.rs`:**
   ```rust
   impl Runtime {
       pub fn heap_stats(&self) -> &GcStats;
       pub fn heap_snapshot(&mut self) -> HeapSnapshot;
       pub fn force_gc(&mut self);   // debug-only; calls collect_full
   }
   ```
4. **Counter wiring.** Increment `alloc_count_total` and `live_bytes`
   in `GcHeap::alloc`; decrement on sweep. Bump `gc_cycles` and
   `last_gc_pause_ms` (using `std::time::Instant`) in `collect_full`.

## Tests

- `stats_round_trip.rs`: alloc 100 of type A; live count is 100;
  drop them, force GC; live count is 0.
- `snapshot_finds_root_to_leaf.rs`: A→B→C cycle plus root → A; verify
  `edges` lists all three pairs and `retained_size(root)` covers all
  three.
- `runtime_heap_stats_visible.rs`: integration test that runs a JS
  snippet via `Runtime::run_script`, calls `runtime.heap_stats()`,
  asserts `live_objects > 0`.

## Out of scope

- Chrome DevTools `.heapsnapshot` writer — listed in §7.1 as
  *optional*. Defer until production-debug demand surfaces.
- Allocation-site sampling (Phase 4).

## Validation gates

- [ ] All three tests pass.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `force_gc` is gated behind `#[cfg(any(test, debug_assertions))]`
  **or** documented as a debug-only API in the public docstring (no
  capability flag yet — file as a follow-up if needed).

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 74 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
