# Task 82 — Migrate `Promise`, `Iterator`, generator state

## Status

- [x] `JsPromiseHandle::Pure` body migrated to `Gc<…>`
- [x] `IteratorState` migrated to `Gc<…>` (all variants)
- [x] generator-frame state migrated to `Gc<…>`
- [x] `Rc<RefCell<…>>` removed from `promise.rs`, iterator paths, generator path
- [x] parked async/generator frames trace correctly
- [x] no parked JS frame or VM handle can be captured by a Rust host future
- [x] gates green

Closed 2026-05-05. Test262 parity was not part of this task's closing
gate.

## Goal

Closes the per-`Value`-variant migrations for Promise/Iterator/Generator.
These three were bundled because they share parked-frame root tracing:
async and async-generator suspension now park isolate-owned frames in
GC-traced promise reactions / microtasks instead of hidden Rust cells.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1,
§4.2 (parked frames as roots), §8 Phase 1 step 6.

## Scope

1. **`PurePromiseBody`** in `promise.rs`:
   ```rust
   pub struct PurePromiseBody {
       state: PromiseState,                 // Pending / Fulfilled / Rejected
       value: Value,
       reactions: Vec<PromiseReaction>,
   }
   impl Traceable for PurePromiseBody { … traces value + each reaction's handler/capability … }
   ```
2. **`IteratorState`** in `lib.rs` — stored as `Gc<IteratorState>`.
   Iterator values and iterator-helper wrapper states use the same
   GC handle shape.
3. **Generator frame state** in `generator.rs` — frame body becomes
   `Gc<GeneratorBody>`. Trace the suspended frame's locals + register
   window + `this`.
4. **Trace `trace_value` arms** for `Promise`, `Iterator`,
   `Generator`.
5. **Parked-frame trace.** In task 75 we stubbed
   `RuntimeState::trace_roots` to walk parked frames. Now make those
   stubs functional: a parked async frame holds locals + register
   values that may include `Gc<…>` handles; trace each. Parked JS
   frames are isolate-owned roots, not Rust futures. Host async work
   receives only copied owned data and an op id; completion posts an
   owned message back to the isolate.

## Out of scope

- Host-bridged Promise variants (`JsPromiseHandle::Host(…)`) — they
  are owned by embedders and trace through their own ABI; out of
  Phase 1 scope.
- BoundFunction / NativeFunction (task 83).

## Validation gates

- [x] No `Rc<RefCell<IteratorState>>` / `Rc<RefCell<PurePromiseBody>>`
  remaining.
- [x] Compile-fail test proves `Frame`, `Value`, `Gc<T>`, and
  `Local<'gc, T>` cannot be captured by a `tokio::spawn` host future.
- [x] All Promise / async / iterator / generator engine fixtures
  pass.
- [x] Regression test
  `tests/gc_promise_iterator_generator.rs::deep_promise_chain_is_reaped_when_unrooted`:
  build a 100k-deep promise chain, drop the root, force GC, assert
  reaped.
- [x] Regression test
  `tests/gc_promise_iterator_generator.rs::promise_iterator_generator_cycles_reclaimed_when_unrooted`:
  generator self-capture and promise/iterator cycles are reclaimed
  when unrooted.
- [x] `gc_roots.rs::microtask_payload_root_survives_force_gc` and
  `gc_roots.rs::parked_frame_keeps_alive` enabled.
- [x] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 82 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
