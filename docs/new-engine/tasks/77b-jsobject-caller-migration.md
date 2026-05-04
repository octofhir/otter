# Task 77B — `JsObject` caller migration sweep

## Status

- [x] every caller of `JsObject` storage methods threads
      `&self.gc_heap` / `&mut self.gc_heap` (or `&NativeCtx`) through
- [x] zero `borrow()` / `borrow_mut()` on `ObjectBody` anywhere in
      `crates-next/otter-vm`
- [x] zero `with_thread_default*` calls in product code (audit gate)
- [x] `cargo build --workspace --all-features` clean
- [x] `cargo test -p otter-gc -p otter-vm -p otter-runtime` green
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean

Closed 2026-05-05.

## Goal

Mechanical follow-up to task 77A. 77A landed the body+trace inside
`object.rs`; 77B threads `&[mut] GcHeap` through every call site so
the workspace builds again. Pure plumbing; no design decisions.

Expected scale (estimate from the reverted WIP run): ~150–400
distinct call sites across ~9–29 files. Land in batches by file and
commit per batch — long-running diffs invite merge pain.

## Source

- [`77a-jsobject-body-and-trace.md`](./77a-jsobject-body-and-trace.md) — API contract.
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md) — explicit-context rule.
- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)

## Scope

Migrate every file under `crates-next/otter-vm/src/` that calls into
`JsObject`. Suggested batching order (smallest-blast-radius first):

1. **Batch 1 — leaf modules** (`error_classes.rs`, `regexp_prototype.rs`,
   `boolean_prototype.rs`, `symbol_prototype.rs`, `string_prototype.rs`).
2. **Batch 2 — dispatch sub-trees** (`binary/dispatch.rs`,
   `string_dispatch.rs`, `symbol_dispatch.rs`,
   `promise_dispatch.rs`).
3. **Batch 3 — namespaces** (`object_statics.rs`, `array_statics.rs`,
   `reflect.rs`, `proxy.rs`, `atomics.rs`, `microtask.rs`).
4. **Batch 4 — sub-crates and built-in modules**
   (`json/`, `intl/`, `temporal/`, `math/`, `number/`, `bigint/`,
   `date/`, `collections.rs`, `collections_prototype.rs`, `regexp.rs`).
5. **Batch 5 — interpreter core** (`lib.rs`).

**Patterns to apply uniformly**:

- `obj.get(key)` → `obj.get(&self.gc_heap, key)` (read) or
  `obj.get(&mut self.gc_heap, key)` only if mutation is required by
  side-effects of `[[Get]]` (e.g. lazy property materialization).
- `obj.set(key, value)` → `obj.set(&mut self.gc_heap, key, value)`.
- For closures that need split borrows, hoist the `&mut self.gc_heap`
  borrow above the `match` / `Entry::or_default()` site:
  ```rust
  let heap = &mut self.gc_heap;
  let entry = self.function_user_props.entry(fn_id).or_default();
  entry.do_thing(heap, …);
  ```
- Where a function takes `&mut self` on `Interpreter` and needs both
  `&mut Interpreter` and `&mut GcHeap`, use the
  `Interpreter::gc_heap_for_cx_mut(&mut self) -> &mut GcHeap`
  accessor introduced in 76A. Do not hand out `&mut Interpreter` and
  `&mut GcHeap` simultaneously.

**Anti-patterns**:

- Do not introduce a free `pub fn obj_get(o, k)` shim that hides the
  heap. ADR-0005 forbids it.
- Do not call `Interpreter::gc_heap_for_cx[_mut]` from outside the
  Interpreter's own dispatch / native-binding glue. Native bindings
  go through `&NativeCtx`.
- Do not `clone()` the heap.

## Out of scope

- Body / trace design — that was 77A.
- Tests / un-ignoring root smoke tests / deleting transitional shims
  — that is 77C.
- Any `JsArray` / `JsMap` / `JsSet` migration.

## Validation gates

- [ ] `cargo build --workspace --all-features` clean.
- [ ] `cargo test -p otter-gc -p otter-vm -p otter-runtime` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm/src crates-next/otter-runtime/src`
      returns zero non-doc hits.
- [ ] `rg "borrow\(\)|borrow_mut\(\)" crates-next/otter-vm/src` returns
      zero hits in any file that touches `JsObject` (exceptions: shape
      transition table, intrinsics-init `RefCell`, anything that
      legitimately wraps non-`ObjectBody` state).

## Closing

Tick 77B in [70-gc-master-tracker.md](./70-gc-master-tracker.md).
77C closes immediately after with the test layer + shim deletion.
