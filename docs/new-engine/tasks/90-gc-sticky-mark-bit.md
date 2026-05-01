# Task 90 — Phase 3: sticky mark-bit minor cycles

## Status

- [ ] open after Phase 2 (task 86) closes

## Goal

V8 optimisation: keep old-gen mark bits across cycles ("sticky"),
so a minor full GC only re-traces newly-allocated objects + objects
whose slots got dirtied by the write barrier. Avoids re-walking the
entire old-gen graph on every cycle. Big throughput win on
steady-state long-lived workloads (servers, REPL sessions).

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §3
(V8 sticky-mark-bit minor cycles), §8 Phase 3.

## Sketch

1. After a full mark cycle finishes, **don't** clear old-gen mark
   bits.
2. Subsequent allocations / barrier-dirtied slots become marker
   roots for the next cycle.
3. Periodically (e.g. every 8th cycle, or after Mark-Compact)
   reset all old-gen marks for full re-validation.
4. Card table from task 72 is the natural source of "dirtied
   since last cycle"; pretenuring counters from task 86.4 inform
   "newly allocated".

## Open questions

1. Reset frequency: heuristic vs. fixed period?
2. Interaction with Mark-Compact (task 88): compaction must clear
   marks anyway since pages move.
3. Worst-case correctness: a missed slot in the dirty set means a
   live object gets reaped. Test budget heavy.

## Validation gates

- Throughput improvement ≥ 20 % on a steady-state long-running
  benchmark.
- Zero regression on cycle reclamation tests.
- Stress test: 1000 cycles with sticky mark-bit on; assert no live
  object incorrectly reaped.

## Closing

Placeholder until Phase 2 ships.
