# Task 75 — Root enumeration: trace every strong reference holder

## Status

- [ ] `RuntimeState::trace_roots(&self, v: &mut dyn FnMut(GcRaw))`
- [ ] `Frame::trace` (locals, register window, accumulator, `this`)
- [ ] microtask queue trace
- [ ] module env trace
- [ ] dynamic-import host trace
- [ ] symbol registry trace
- [ ] one regression test per root type
- [ ] gates green

## Goal

Stand up the root walker that `GcHeap::collect_full` will consume.
Without this in place, the migrations in tasks 76–83 will silently
reap live objects (they appear unrooted to the GC) and the engine
will explode in undebuggable ways. **This task lands before any value
type migrates.**

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.2
(root sources), §4.3 (pseudocode).

## Scope

1. **Trace stub on each VM type that will move to GC.** At this
   stage the types are still `Rc<RefCell<…>>`-shaped — the trace
   functions take `&self` and walk *whichever* fields will become
   `Gc<…>` after task 76+. They do nothing today, but exist so the
   migration tasks add bodies, not signatures.
2. **`RuntimeState::trace_roots`** — central walker called by the
   GC. Lives in a new file `crates-next/otter-vm/src/runtime_state.rs`
   (carves out the state types currently inlined in `lib.rs`).
   Enumerates:
   - `globals: JsObject`
   - `intrinsics: Intrinsics`
   - `module_environments` values
   - active call frames (locals + accumulator + register window
     + `this` + bytecode-module reference)
   - parked async/generator frames in promise reactions
     (`Rc<Cell<Option<Box<Frame>>>>` slots in `lib.rs:4417, 4452`)
   - microtask queue
   - dynamic-import host (`module_loader::DYNAMIC_IMPORT_HOST`)
   - symbol registry
3. **Smoke test scaffold.** A `crates-next/otter-vm/tests/gc_roots.rs`
   integration test that — for each root type — allocates a value,
   stashes it via that root path, drops the local handle, calls
   `runtime.force_gc()`, asserts the value is still readable. Since
   the migration is not yet done, the assertion uses **the future
   `Gc<T>` API** — this test starts as `#[ignore]` with a TODO and
   gets un-ignored as each migration task lands.

## Out of scope

- Migrating any value type to `Gc<T>` (tasks 76+).
- Removing `RefCell` from public APIs (lands per-type in tasks 76+).
- Making the trace bodies non-empty for types not yet migrated.

## Validation gates

- [ ] `RuntimeState::trace_roots` exists and is called by
  `GcHeap::collect_full` whenever the runtime triggers it.
- [ ] Each future-`Gc` type has an empty `trace` method waiting for
  its migration task.
- [ ] `cargo test --workspace` green; no behaviour change.
- [ ] One `#[ignore]` smoke test per root listed in scope (these
  un-ignore in tasks 76–83).

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 75 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
