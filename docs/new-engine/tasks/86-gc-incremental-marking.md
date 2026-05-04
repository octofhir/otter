# Task 86 — Phase 2: incremental marking + concurrent sweep + pretenuring

## Status

- [ ] open after Phase 1 (task 84) closes; do not pick up before that
- [ ] task 85 event-loop / isolate-runner design reviewed before
      choosing step scheduling hooks

## Goal

Move the new engine from "STW old-gen + STW sweep" to V8/JSC-shaped
"incremental marking + background concurrent sweep + allocation-site
pretenuring". Closes the production pause-time target (≤ 5 ms
steady-state at 1 GB live;
[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.2 NF1)
and brings sweep off the mutator thread.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §3
(V8 / JSC technique table), §5 (insertion barrier), §8 Phase 2.

## Why this batch is small

- Tri-color worklist + `drain_with_budget` already shipped in task
  72.
- Insertion-barrier *call sites* already wired in tasks 76–83
  (every pointer store calls `heap.write_barrier`). Phase 2 flips
  `is_marking` so the existing paths go load-bearing. **No new
  audit sweep across `otter-vm`.**
- Black allocation already wired in task 72 alloc fast path.

## Scope (split into sub-tasks before starting)

### 86.1 — Incremental marking driver

- `IncrementalMarker` cycle state machine: idle → marking → finishing.
- Step budget per back-edge tick (default 1 ms wall, adaptive).
- Cycle start: snapshot roots, set `is_marking = true`, push roots
  to worklist.
- Each back-edge: `marking.drain_with_budget(budget)`. Returns
  `true` when worklist empty.
- Event-loop integration: ADR-0005's isolate runner may also call the
  same budgeted step at safe runtime turns (timer wake, host-op
  completion, command boundary). The marker still runs on the isolate
  mutator; Tokio worker threads never touch `GcHeap`.
- Cycle finish: STW finalisation pass to drain straggler grays
  (insertion barrier guarantees no live whites remain), schedule
  sweep.

### 86.2 — Concurrent sweeping

- Background sweeper thread launched at `GcHeap::new`. Tokio exists in
  the product runtime, but the sweeper must remain a narrow GC-owned
  Sync surface; std `thread` or a dedicated blocking task is acceptable
  if the ownership contract is explicit.
- After mark-finalisation, mutator hands the swept-page list to the
  sweeper via a lock-free queue.
- Foreground alloc parks on the alloc fast path **only** when it
  hits a partially-swept page; the sweeper publishes a per-page
  ready-flag.
- Old-space free-list rebuild happens on the sweeper thread.

### 86.3 — Incremental sweeping (foreground complement)

- For pages outside the concurrent-sweep budget, drive a foreground
  incremental sweeper from the same back-edge tick. Uses the same
  `drain_with_budget` shape as marking.

### 86.4 — Allocation-site pretenuring

- Each `Op::AllocObject` / `Op::AllocArray` carries a 16-bit
  `alloc_site_id`.
- `GcHeap` keeps a per-site counter: total allocs, survivors-after-N
  scavenges, last-N-cycle survival ratio.
- After threshold (V8 default: 60 % survival across 3 cycles)
  the runtime allocates that site directly to old-gen.
- Compiler emits `alloc_site_id` in the bytecode; runtime threads it
  into `alloc()`.

## Open questions

1. Step budget: fixed at 1 ms or adaptive based on heap-growth rate?
2. SATB (snapshot-at-the-beginning) vs incremental update: Phase 1
   barrier is Dijkstra (incremental update). SATB needs a different
   shape; defer unless concurrent marking lights up.
3. Cycle scheduling: trigger when old-gen reaches 80 % of next
   threshold, or when `tracked_bytes` crosses?
4. Concurrent sweep crash safety: if the sweeper panics, the heap
   must surface it through the next safepoint, not abort.

## Validation gates — production-grade bar

### Pause-time

- [ ] 99p mutator-thread pause ≤ 5 ms at 1 GB live (architecture
  doc §1.2 NF1).
- [ ] 99.9p mutator-thread pause ≤ 20 ms at 1 GB live.
- [ ] Sweep no longer appears in the mutator-thread pause histogram
  (concurrent sweeper takes it).
- [ ] Histograms captured on:
  - test262 Promise / async corpus
  - long-running allocation-heavy embedder workload (24 h)

### Throughput

- [ ] Allocation throughput within 5 % of Phase 1 baseline.
- [ ] **Throughput-parity bar (architecture doc §1.2 NF10):** end-to-end
  on a curated benchmark suite (object-literal bursts, closure
  chains, JSON parse, async/await chains) within **30 % of V8
  Node.js current LTS** at the same pause-time SLO. Sub-50 %
  triggers a perf-track review before Phase 3 starts.
- [ ] Pretenuring: ≥ 30 % reduction in young-gen scavenge count on
  allocation-heavy benchmarks vs. Phase 1 baseline.

### Correctness

- [ ] No regression in cycle reclamation / WeakMap eviction /
  WeakRef / FinalizationRegistry tests from Phase 1.
- [ ] No Tokio task, `RuntimeHandle`, or host-op future can call into
  `GcHeap` directly; incremental marker entrypoints are reachable only
  from the isolate runner / mutator or the dedicated sweeper hand-off.
- [ ] **`loom` model-checking pass** on the concurrent-sweeper
  hand-off queue and the alloc-fast-path / sweeper-park
  interaction. No race conditions.
- [ ] **Concurrent-sweep crash safety:** sweeper-thread panic
  surfaces through the next safepoint as `OtterError::Internal` —
  process does not abort.

### Memory safety

- [ ] miri green on the GC test set (miri does not model threads
  perfectly; sweeper tests use a single-thread mode for miri).
- [ ] ThreadSanitizer build green on the concurrent path:
  `RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test -p otter-gc --features concurrent-sweep`.

## Closing

When ready to start, slice 86.1–86.4 into separate task files. Until
then this file is the master placeholder.
