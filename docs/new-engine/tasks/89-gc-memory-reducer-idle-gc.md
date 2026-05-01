# Task 89 — Phase 3: memory reducer / idle GC

## Status

- [ ] open after Phase 2 (task 86) closes

## Goal

Add proactive GC triggered on idle callbacks — V8 standard
("MemoryReducer" / "IncrementalMarkingJob"). Prevents long-tail RSS
growth in long-running embedders that allocate sporadically and
never cross the regular GC threshold.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §8
Phase 3.

## Sketch

1. `Runtime::notify_idle(deadline_ms: u32)` — embedder hook called
   when the host loop has spare time.
2. Memory reducer state machine: `Done → Wait → Run → Done`.
   Triggers a full GC if heap has grown ≥ 10 % since last full GC
   AND mutator has been idle ≥ 1 s.
3. Best-effort: if `deadline_ms` runs out mid-cycle, the
   incremental marker (task 86) yields back to the embedder.
4. Add `Runtime::set_idle_policy(IdlePolicy)` for tuning:
   `Aggressive` (1 s idle threshold) / `Balanced` (default,
   10 s) / `Manual` (no idle GC).

## Open questions

1. Idle hook ABI: `notify_idle(deadline)` return value — what does
   the embedder do with it?
2. Default policy for the CLI vs. embedded use cases.

## Validation gates

- Long-running idle workload (sleep + occasional alloc) shows
  steady-state RSS instead of monotonic growth.
- No regression in throughput benchmarks (idle GC only fires when
  truly idle).

## Closing

Slice when ready. Placeholder.
