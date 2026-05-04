# Task 81 — `WeakRef` and `FinalizationRegistry`

## Status

- [ ] `WeakRef` value variant + intrinsic
- [ ] `FinalizationRegistry` value variant + intrinsic
- [ ] post-sweep finaliser dispatch through microtask queue
- [ ] finaliser enqueueing is isolate-local and uses explicit runtime
      context
- [ ] gates green

## Goal

Wire ECMA-262 §26.1 `WeakRef` and §26.2 `FinalizationRegistry`
through the GC sweep hook. The new engine doesn't have these types
today — they need both the type definitions and the sweep-time
callback dispatch that only a tracing GC can provide.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.1
F2/F3, §6.2 (Drop semantics during sweep), §10.2 Q3.

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
5. **Spec links** in module docstrings:
   `https://tc39.es/ecma262/#sec-weak-ref-objects`,
   `https://tc39.es/ecma262/#sec-finalization-registry-objects`.

## Out of scope

- `cleanupSome` (Stage-3, not in core spec).
- Synchronous finalisation. ECMA-262 mandates async via the host
  task queue; we already have a microtask queue.

## Validation gates

- [ ] `WeakRef.prototype.deref` returns the target while live;
  returns `undefined` after the target is reaped.
- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm crates-next/otter-gc/src/finalize.rs` returns no WeakRef / finalisation product-code hits.
- [ ] `FinalizationRegistry` callback fires *exactly once* per cell
  whose target is reaped.
- [ ] Cycle test: registry holds a callback that captures itself
  through the held value; drop the registry; assert no leak.
- [ ] Test262 `built-ins/WeakRef/**` and
  `built-ins/FinalizationRegistry/**` pass at parity-or-better with
  whatever baseline existed (likely ~0 today since the types didn't
  exist).
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 81 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
