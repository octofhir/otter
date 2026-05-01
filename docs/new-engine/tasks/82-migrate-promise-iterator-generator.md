# Task 82 — Migrate `Promise`, `Iterator`, generator state

## Status

- [ ] `JsPromiseHandle::Pure` body migrated to `Gc<…>`
- [ ] `IteratorState` migrated to `Gc<…>` (all 7 variants)
- [ ] generator-frame state migrated to `Gc<…>`
- [ ] `Rc<RefCell<…>>` removed from `promise.rs`, iterator paths, generator path
- [ ] parked async/generator frames trace correctly
- [ ] gates green

## Goal

Closes the per-`Value`-variant migrations for Promise/Iterator/Generator.
These three are bundled because they share the *parked frame* root
pattern (`Rc<Cell<Option<Box<Frame>>>>` slots in `lib.rs:4417, 4452`)
that task 75 stubbed out — this task fills in the actual trace
bodies.

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
2. **`IteratorState`** in `lib.rs` — currently
   `Rc<RefCell<IteratorState>>` in 7 places. Replace with
   `Gc<IteratorState>`. The 7 `Value` variants
   (`Iterator`, ArrayIterator, MapIterator, etc.) all use the same
   `Gc<IteratorState>` handle.
3. **Generator frame state** in `generator.rs` — frame body becomes
   `Gc<GeneratorBody>`. Trace the suspended frame's locals + register
   window + `this`.
4. **Trace `trace_value` arms** for `Promise`, `Iterator`,
   `Generator`.
5. **Parked-frame trace.** In task 75 we stubbed
   `RuntimeState::trace_roots` to walk parked frames. Now make those
   stubs functional: a parked async frame holds locals + register
   values that may include `Gc<…>` handles; trace each.

## Out of scope

- Host-bridged Promise variants (`JsPromiseHandle::Host(…)`) — they
  are owned by embedders and trace through their own ABI; out of
  Phase 1 scope.
- BoundFunction / NativeFunction (task 83).

## Validation gates

- [ ] No `Rc<RefCell<IteratorState>>` / `Rc<RefCell<PurePromiseBody>>`
  remaining.
- [ ] All Promise / async / iterator / generator engine fixtures
  pass.
- [ ] Regression test `tests/gc_promise_chain.rs`: build a 100k-deep
  promise chain, drop the root, force GC, assert reaped.
- [ ] Regression test `tests/gc_generator_capture.rs`: a generator
  whose locals capture itself; drop the outer ref; force GC; assert
  reaped.
- [ ] `gc_roots.rs::microtask_queue_keeps_alive` and
  `parked_frame_keeps_alive` un-ignored.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 82 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
