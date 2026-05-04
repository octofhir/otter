# Task 87 — Phase 3 (deferred): concurrent marking + compaction

## Status

- [ ] **deferred indefinitely.** Do not pick up unless production
  embedders demand it.

## Goal

Move marker work off the mutator thread entirely (V8 concurrent
marking, JSC parallel marking). Add old-gen compaction to claw back
fragmentation.

## Why deferred

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §3 V8/JSC
table row "concurrent marking — deferred indefinitely":

> Multi-threaded marking implies tri-color CAS plus a parking-mutator
> protocol. Not justified at our heap sizes.

One isolate = one mutator (ADR-0005 / NF5). Public `RuntimeHandle`
clones may be used from many Tokio worker threads, but that does not
permit those workers to touch the heap. Concurrent marking buys
single-digit-ms pause headroom; the complexity cost (parking protocol,
atomic mark CAS, race-windows in barrier paths) is large. Pick up when
*production* embedders observe Phase-2 incremental pauses are still over
budget under sustained load — not before.

Compaction similarly deferred: relevant only after months of
production use surface real fragmentation.

## What lands here, when picked up

1. Concurrent marker thread; mutator parking at safepoints.
2. CAS-shaded mark transitions (`AtomicU8` flags already in place
   from task 72).
3. Barrier paths audited for races (the Phase 1 barriers were
   single-threaded; concurrent marking exposes the parent/child
   load ordering).
4. Old-gen compaction with forwarding tables.
5. ADR-0005 amendment if any new public API, thread-affinity rule, or
   unsafe surface lands.

## Closing

This file remains a deliberate placeholder. Touch only when
re-opening.
