# Task 88 — Phase 3: Mark-Compact for old-gen fragmentation

## Status

- [ ] open after Phase 2 (task 86) closes

## Goal

Add a Mark-Compact (sliding compactor) pass for old-gen pages that
have crossed a fragmentation threshold. Reclaims free-list slack
that mark-sweep alone cannot. Matches V8 since 2014 and JSC's
`MarkedBlock::sweepCompact`.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §3
(V8 / JSC table — Mark-Compact / compaction), §8 Phase 3.

## Sketch

1. After mark phase, identify candidate pages: free-list slack >
   30 % (V8 heuristic).
2. Compute compaction target: a fresh page or another candidate's
   live-block start.
3. Sliding compactor: forward each live object to its new address;
   record forwarding in `GcHeader::set_forwarding_address` (already
   shipped in task 72).
4. Update every pointer (mutator slots + remembered set + handle
   stack + globals) to forwarded addresses. Reuse the scavenger's
   slot-update infrastructure.
5. Free old pages.

## Open questions

1. Compaction frequency: every Nth full GC, or threshold-driven?
2. Pinning: large objects in `LargeObjectSpace` are inherently
   pinned; mark-compact must skip them (already covered by `is_pinned`
   flag in `GcHeader`).
3. Concurrent compaction (V8 has it) — defer to Phase 4.

## Validation gates

- Old-gen RSS after long-running embedder workload (1 hour
  steady-state) drops to within 10 % of live-set size.
- No regression in functional tests.
- Pause-time histogram acceptable: STW compaction ≤ 100 ms at 1 GB
  live (acceptable since it runs rarely).

## Closing

Slice into sub-tasks when ready. Until then placeholder.
