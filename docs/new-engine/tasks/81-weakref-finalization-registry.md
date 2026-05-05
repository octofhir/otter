# Task 81 — `WeakRef` and `FinalizationRegistry`

## Status

- [x] `WeakRef` value variant + intrinsic
- [x] `FinalizationRegistry` value variant + intrinsic
- [x] post-sweep finaliser dispatch through microtask queue
- [x] finaliser enqueueing is isolate-local and uses explicit runtime
      context
- [x] finalizer/resurrection safety policy documented and tested
- [x] allocation-during-finalizer path uses pending queues / black
      allocation discipline and never mutates active sweep queues
- [x] gates green except Test262 parity, intentionally deferred

## Goal

Wire ECMA-262 §26.1 `WeakRef` and §26.2 `FinalizationRegistry`
through the GC sweep hook. The new engine doesn't have these types
today — they need both the type definitions and the sweep-time
callback dispatch that only a tracing GC can provide.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.1
F2/F3, §6.2 (Drop semantics during sweep), §10.2 Q3.
Research input: Boa Oscars' collection ordering notes: finalize while
dead allocations are still inspectable, re-check liveness/rooting
before freeing, and route allocations made during collection into
pending queues. Adapt the idea to ECMAScript by enqueueing JS cleanup
callbacks after sweep, never running JS during raw GC.

## Scope

1. **`WeakRef`** — `Value::WeakRef(Gc<WeakRefBody>)` where
   `WeakRefBody { target: GcRaw }`. `Traceable::trace` is a no-op;
   the sweep walks weak-ref registry separately and clears `target`
   if unmarked.
2. **`FinalizationRegistry`** —
   `Value::FinalizationRegistry(Gc<FinalizationRegistryBody>)` with:
   ```rust
   pub struct FinalizationRegistryBody {
       cleanup_callback: Value,             // callable; STRONG
       cells: Vec<FinalizerCell>,
   }
   pub struct FinalizerCell {
       target: GcRaw,                       // WEAK
       held_value: Value,                   // STRONG
       unregister_token: Option<GcRaw>,     // WEAK
   }
   ```
   `Traceable::trace` traces `cleanup_callback` and every
   `held_value` — but **not** `target` or `unregister_token`.
3. **GcHeap weak-ref registry.** `GcHeap` keeps a `Vec<GcRaw>` of
   live `WeakRef` and `FinalizationRegistry` handles. Registered on
   alloc, removed on sweep.
4. **Post-sweep finaliser dispatch.** After the heap sweep:
   - For each `WeakRef` whose `target` got swept: clear it.
   - For each `FinalizationRegistry`, find cells whose `target` got
     swept, queue their `held_value` to the registry's callback via
     `MicrotaskKind::FinalizationCallback`.
   The callback is queued on the isolate's microtask queue and runs on a
   later mutator turn. Do not run JS callbacks during sweep, and do not
   enqueue through a Tokio worker or thread-local heap lookup.
5. **Finalizer safety policy.**
   - Raw GC finalization may inspect liveness metadata, clear weak
     handles, and enqueue jobs only.
   - It must not call into the JS interpreter, property accessors,
     promise reactions, native user callbacks, or Tokio worker tasks.
   - If a Rust-side drop/finalize path allocates while `is_collecting`
     is true, the allocation is either black-allocated for the current
     cycle or recorded in a pending queue that is appended after the
     active sweep queue drains.
   - Before a slot is physically freed, re-check whether any allowed
     finalizer action re-rooted or otherwise made it reachable. JS
     `FinalizationRegistry` callbacks themselves run later, so they
     cannot resurrect the just-swept target during the raw sweep.
6. **Registry laziness.** Keep the architecture-doc Q3 answer: a
   per-heap `has_registries` / weak-finalization registry flag should
   make the post-sweep walk a near-zero-cost branch when no
   `FinalizationRegistry` exists.
7. **Spec links** in module docstrings:
   `https://tc39.es/ecma262/#sec-weak-ref-objects`,
   `https://tc39.es/ecma262/#sec-finalization-registry-objects`.

## Out of scope

- `cleanupSome` (Stage-3, not in core spec).
- Synchronous finalisation. ECMA-262 mandates async via the host
  task queue; we already have a microtask queue.

## Validation gates

- [x] `WeakRef.prototype.deref` returns the target while live;
  returns `undefined` after the target is reaped.
- [x] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm crates-next/otter-gc/src/finalize.rs` returns no WeakRef / finalisation product-code hits.
- [x] `FinalizationRegistry` callback fires *exactly once* per cell
  whose target is reaped.
- [x] Cycle test: registry holds a callback that captures itself
  through the held value; drop the registry; assert no leak.
- [x] Resurrection policy test: cleanup callback cannot observe or
  resurrect the collected target through `WeakRef.deref`.
- [x] Allocation-during-finalization test: a Rust-side finalizer path
  allocates while collection is closing; force GC and assert queue
  ordering, no UAF, and no stale mark bits.
- [x] Registry-laziness test: with no registries allocated, post-sweep
  weak-finalization work is skipped except for a single flag check.
- [~] Test262 `built-ins/WeakRef/**` and
  `built-ins/FinalizationRegistry/**` pass at parity-or-better with
  whatever baseline existed. Deferred by maintainer direction until
  the surrounding JS surface is ready.
- [x] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Closed 2026-05-05 for the non-Test262 task-81 vertical slice. Test262
parity remains deferred because constructor/prototype conformance
depends on broader JS surface work beyond this GC slice.
