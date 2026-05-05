# Task 80 — `WeakMap` / `WeakSet` with ephemeron fixpoint

## Status

- [ ] `JsWeakMap = Gc<WeakMapBody>` / `JsWeakSet = Gc<WeakSetBody>`
- [ ] ephemeron table replaces strong-ref entries
- [ ] `GcHeap::collect_full` exposes `mark_phase` / `mark_additional` / `sweep_phase` split
- [ ] post-mark fixpoint passes ephemeron values into worklist
- [ ] dead-key entry sweep before slot sweep
- [ ] ephemeron mutation and fixpoint APIs use explicit runtime / heap
      context; no thread-local heap lookup
- [ ] weak-map registry owns pruning metadata and handles replacement /
      removal invalidation without leaking stale ephemeron nodes
- [ ] allocation-during-GC path preserves ephemeron/root queue ordering
      and does not mutate the active sweep queue
- [ ] `task 57` markers in `collections.rs` removed
- [ ] gates green

## Goal

Today `WeakMap` / `WeakSet` keep **strong** refs (`collections.rs:6,
408` — comments say "until task 57 lands the tracing GC"). This task
is task 57. Implements proper ephemeron semantics so a `WeakMap` entry
becomes unreachable once its key is unreachable through any other
path.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.1
F2, §2.1 ("Ephemeron fixpoint API split"), §10.1 R3, §10.2 Q3.
Reference impl: legacy `crates/otter-gc/src/typed.rs`
`run_mark_phase` / `run_mark_additional` / `run_sweep_phase`.
Research input: Boa Oscars' weak-map / ephemeron prototype. We take
the registry, invalidation, pruning, and test ideas; we do **not** take
its non-moving mark-sweep arena backend.

## Scope

1. **`EphemeronTable`** in `crates-next/otter-gc/src/ephemeron.rs`:
   ```rust
   pub struct EphemeronTable {
       entries: Vec<(GcRaw /* key */, Value /* value */)>,
   }
   impl EphemeronTable {
       pub fn set(&mut self, key: GcRaw, val: Value);
       pub fn get(&self, key: GcRaw) -> Option<&Value>;
       pub fn delete(&mut self, key: GcRaw) -> bool;
   }
   ```
2. **GcHeap split-mark API:**
   ```rust
   impl GcHeap {
       pub fn mark_phase(&mut self, roots: impl IntoIterator<Item = GcRaw>);
       pub fn mark_additional(&mut self, additions: impl IntoIterator<Item = GcRaw>);
       pub fn sweep_phase(&mut self);   // closes the cycle
       pub fn is_marked(&self, gc: GcRaw) -> bool;
   }
   ```
   `collect_full` is now `mark_phase + ephemeron_fixpoint +
   sweep_phase` in sequence.
3. **Ephemeron fixpoint** run between mark and sweep: for each
   `WeakMap`/`WeakSet`, for each entry whose key is `is_marked`, call
   `mark_additional([value])`. Iterate until no new objects mark
   (fixed point). Then sweep dead-key entries from each weak
   collection's table before the heap sweep runs. The fixpoint driver is
   called by the isolate mutator while holding explicit `RuntimeCx` /
   `&mut GcHeap`; it is not an async task and never runs from Tokio
   worker threads.
4. **`WeakMapBody` / `WeakSetBody`** become `EphemeronTable`-backed.
   Their `Traceable::trace` is **a no-op** (the GC consults the
   ephemeron table separately during fixpoint).
5. **Registry.** `GcHeap` keeps a `Vec<GcRaw>` of live ephemeron
   tables (registered on alloc, removed on sweep) so the fixpoint
   pass knows what to consult.
6. **Replacement / removal invalidation.** Replacing
   `WeakMap[k] = old` with `WeakMap[k] = new` must invalidate the old
   ephemeron entry immediately. `delete(k)` must remove the table entry
   and invalidate the backing ephemeron node; the node itself can be
   reclaimed during the next sweep. This mirrors Oscars' useful
   invariant: map lookup state and collector-owned ephemeron storage
   are separate, so stale backing nodes do not keep values alive.
7. **Weak-map registry pruning.** The registry walks live weak
   collections before freeing heap slots, while dead keys and dead
   ephemeron nodes can still be inspected safely. Dead maps unregister
   their metadata without relying on `Drop` ordering of JS objects.
8. **Allocation during collection.** If a finalizer or host hook
   allocates while a GC cycle is closing, new roots/ephemerons go into
   pending queues and are appended after the active sweep queue drains.
   Do not mutate the active queue while iterating it.
9. **Remove** the `// task 57` markers from `collections.rs:6, 408,
   lib.rs:194`.

## Out of scope

- `WeakRef` / `FinalizationRegistry` (task 81 — they share the
  registry pattern but trigger callbacks).
- Per-entry value finalisers — JS `WeakMap` has no callback; only
  `FinalizationRegistry` does.

## Validation gates

- [ ] All existing collection fixtures pass.
- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm/src/collections.rs crates-next/otter-gc/src/ephemeron.rs` returns no product-code hits.
- [ ] Regression test `tests/gc_weakmap_eviction.rs`: hold a key
  briefly, drop it, force GC, assert the WeakMap entry is gone and
  the value object is reaped.
- [ ] Regression test `tests/gc_ephemeron_chain.rs`: WeakMap[k1]=k2,
  WeakMap[k2]=v; drop k1's only strong ref; assert k2 and v are reaped.
- [ ] Pathological self-ref test: WeakMap[obj]=obj; drop obj; assert
  reaped (no fixpoint loop).
- [ ] Replacement test: set the same key twice, drop the key, force GC,
  assert both the old invalidated ephemeron and new dead-key ephemeron
  are reclaimed.
- [ ] Remove test: set then delete an entry, keep the key briefly, drop
  it, force GC, assert the backing ephemeron is not leaked.
- [ ] Map-drop test: drop the `WeakMap` while keys are still live, force
  GC, assert collector registry metadata is reclaimed and values are not
  retained by stale map state.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 80 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
