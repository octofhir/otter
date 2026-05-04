# Task 77C — `JsObject` regression tests + thread-default shim cleanup

## Status

- [x] `tests/gc_roots.rs::globals_keep_object_alive` un-ignored and passing
- [x] `tests/gc_roots.rs::module_env_keeps_object_alive` un-ignored and passing
- [x] `tests/gc_object_cycle.rs::proto_cycle_reaped` written and passing
- [x] `#[doc(hidden)]` thread-default helpers deleted from `heap.rs`
      (`enter_thread_default`, `install_thread_default`,
      `with_thread_default`, `with_thread_default_mut`,
      `has_thread_default`, `ThreadDefaultGuard`, `THREAD_HEAP`,
      `THREAD_HEAP_BORROWED`)
- [x] task 76A third checkbox tightened from `[~]` to `[x]`
- [x] tasks 77, 77A, 77B, 77C all closed in `70-gc-master-tracker.md`

Closed 2026-05-05.

## Goal

Final slice of task 77. After 77A (body) + 77B (callers) the
workspace builds and runs. 77C proves cyclic graphs are reaped and
removes the last vestige of the thread-default escape hatch from
the GC crate.

## Source

- [`77-migrate-jsobject.md`](./77-migrate-jsobject.md) §Validation gates.
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md) §3.
- [`../gc-architecture.md`](../gc-architecture.md) §4.2 (root sources).

## Scope

1. **Un-ignore root smoke tests.** In
   `crates-next/otter-vm/tests/gc_roots.rs`, remove the `#[ignore]`
   attribute from `globals_keep_object_alive` and
   `module_env_keeps_object_alive`. They were stubbed in task 75 with
   the assumption that `Value::Object` would walk a real
   `Gc<ObjectBody>` slot once 77A landed.

2. **New cycle regression** at
   `crates-next/otter-vm/tests/gc_object_cycle.rs::proto_cycle_reaped`:
   ```rust
   // let a = {};
   // let b = { __proto__: a };
   // a.__proto__ = b;
   // drop a, b;
   // collect_full;
   // assert: only intrinsic objects remain live.
   ```
   Use `Runtime::heap_stats()` / `force_gc()` (from task 74) to
   measure. Assert the count of live `Object` cells drops to the
   intrinsics baseline measured at runtime start.

3. **Delete thread-default shims** from
   `crates-next/otter-gc/src/heap.rs`:
   - the `enter_thread_default` / `install_thread_default` /
     `with_thread_default[_mut]` / `has_thread_default` methods on
     `GcHeap`
   - the `ThreadDefaultGuard` type
   - the `THREAD_HEAP` and `THREAD_HEAP_BORROWED` thread-locals
   - the `use std::cell::Cell` / `use std::ptr::NonNull` lines if
     they are now unused
   The migration notes pointing at task 76A go away with the symbols.

4. **Compile-fail fixture sweep.** Re-run the `trybuild` suite added
   in task 76A. If any new compile-fail case becomes available now
   that thread-default is deleted (e.g. an `obj.get()` without
   `&heap` parameter), add a fixture. Optional but encouraged.

5. **Tracker bookkeeping.** In
   `docs/new-engine/tasks/70-gc-master-tracker.md`:
   - tick 77, 77A, 77B, 77C
   - tighten the 76A row's third checkbox to `[x]` (the
     `[~] product code free of `with_thread_default*` / shims behind
     `pub(crate) testing`` line)
   - delete the "Open work (post-76A)" section from
     `77-migrate-jsobject.md` once 77 is fully closed

## Out of scope

- Anything touching `JsArray` / `JsMap` / `JsSet` — those are tasks
  78, 79.

## Validation gates

- [ ] `cargo test -p otter-vm --test gc_roots` green, no `#[ignore]`.
- [ ] `cargo test -p otter-vm --test gc_object_cycle` green.
- [ ] `cargo test -p otter-gc -p otter-vm -p otter-runtime` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `rg "THREAD_HEAP|with_thread_default|enter_thread_default|install_thread_default|ThreadDefaultGuard" crates-next/otter-gc/src crates-next/otter-vm/src crates-next/otter-runtime/src`
      returns zero hits.
- [ ] `cargo fmt --all -- --check` clean.

## Closing

Tick 77, 77A, 77B, 77C in
[70-gc-master-tracker.md](./70-gc-master-tracker.md). Delete the
`Open work (post-76A)` section from
[77-migrate-jsobject.md](./77-migrate-jsobject.md). Files
77/77A/77B/77C remain in the repo — close-by-tick, not delete-by-PR.
