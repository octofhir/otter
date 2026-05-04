# Task 77A — `JsObject = Gc<ObjectBody>` body + trace

## Status

- [x] `pub type JsObject = Gc<ObjectBody>` in `object.rs`
- [x] `Rc<RefCell<ObjectBody>>` removed from `object.rs`
- [x] `ObjectBody` `SafeTraceable` impl traces prototype + property
      values + symbol props (shape keys are interned, no trace)
- [x] every `borrow()` / `borrow_mut()` on `ObjectBody` deleted
- [x] `JsObject` methods take `&otter_gc::GcHeap` / `&mut otter_gc::GcHeap`
      explicitly; no thread-default access
- [x] `Value::trace_value_slots` `Object` arm dispatches to `v(o.raw())`
- [x] `Value::as_gc_raw` `Object` arm returns `Some(o.raw())`
- [x] write barriers fire on every property store + prototype change
- [x] `cargo build -p otter-vm` breaks callers as expected (179
      errors across ~24 caller files — see 77B input report).
      `otter-gc` builds clean; `otter-vm` library `object.rs`
      itself compiles internally (no errors anchored at the
      module).

## Goal

First slice of task 77. Land the body type swap and tracing inside
`crates-next/otter-vm/src/object.rs` so subsequent caller-migration
slices (77B) only do mechanical edits, not design work. Build of the
workspace will be temporarily red between 77A and 77B; that is the
explicit cost of doing this in order.

This task is the design-load piece: every other slice (77B, 77C) is
mechanical. Spend the time here on the right shape.

## Source

- [`../gc-architecture.md`](../gc-architecture.md) §4.1, §6.3, §8.
- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md) — bans `with_thread_default*` in product code.
- [`76-migrate-upvalue-cell.md`](./76-migrate-upvalue-cell.md) — the migration template (`Gc<UpvalueCellBody>` shape).

## Scope

1. **Body type** in `crates-next/otter-vm/src/object.rs`:
   ```rust
   pub type JsObject = otter_gc::Gc<ObjectBody>;

   pub struct ObjectBody {
       pub(crate) shape: Rc<Shape>,           // immutable; Rc stays
       pub(crate) slots: SmallVec<[PropertySlot; 8]>,
       pub(crate) prototype: Option<JsObject>,
       pub(crate) extensible: bool,
       pub(crate) symbol_properties: …,
       // anything else `ObjectBody` already carried in the
       // pre-77A `Rc<RefCell<…>>` layout
   }
   ```
2. **`SafeTraceable` impl** for `ObjectBody`. Trace:
   - `prototype` (`Option<Gc<ObjectBody>>` → forward `o.raw()` if `Some`).
   - every `Value` inside `slots` (data slots and accessor get/set).
   - every `Value` inside `symbol_properties`.
   - **do not** trace `shape` — shapes hold interned `Rc<JsString>`
     keys, leaves; per §4.1 architecture they are not GC-managed.
3. **Method API** — every method on `JsObject` that today reads or
   writes the body must take `&otter_gc::GcHeap` (read) or
   `&mut otter_gc::GcHeap` (mutate). Pick the **free-function**
   shape from §6.3 of the architecture doc — methods on
   `JsObject` are now thin wrappers: `obj.get(heap, key)` becomes
   `heap.with(obj.raw(), |body| …)` internally. **No
   `with_thread_default*` calls.** No `pub(crate)` shim functions
   that hide the heap parameter.
4. **Write barriers** — every store of a `Gc<…>`-bearing `Value`
   into a `slot`, every prototype assignment, every symbol-property
   write must call `heap.write_barrier(owner.raw(), value)` (or the
   equivalent helper from the GC API). Barriers are inline; do
   not stash them behind a closure.
5. **`Value` plumbing** — extend `Value::trace_value_slots` to
   recurse `Object(o) → v(o.raw())`. Extend `Value::as_gc_raw`
   similarly. (Both helpers were stubbed in task 75/76 with a
   placeholder for `Object`.)
6. **Migration receipt** — leave the `#[doc(hidden)]` thread-default
   shims in `heap.rs` untouched. They are deleted in task 77C.

## Out of scope

- Caller migration in `lib.rs` / `object_statics.rs` / `reflect.rs`
  / etc. — that is task 77B.
- `JsArray`, `JsMap`, `JsSet` — tasks 78, 79.
- Un-ignoring `gc_roots.rs` / writing `gc_object_cycle.rs` — task 77C.
- Deleting the `#[doc(hidden)]` thread-default shims — task 77C.

## Hot-path considerations

- `Rc<Shape>` stays. Shapes are immutable post-transition; sharing
  via `Rc` is correct, GC handles only the body.
- One slot-table indirection per access vs. removed `RefCell`
  borrow check — net even or favourable.
- Inline `heap.write_barrier(…)` at every store; do not box.

## Validation gates

- [x] `cargo build -p otter-gc` clean.
- [x] `cargo build -p otter-vm --lib` fails at callers (179 errors
      across ~24 files; counts handed to 77B). `object.rs` itself
      is **not** the source of any build error.
- [x] `cargo test -p otter-gc` green.
- [x] `rg "borrow\(\)|borrow_mut\(\)" crates-next/otter-vm/src/object.rs`
      returns zero hits. (Shape's lazy `offsets` cache moved to
      `OnceCell` and the `transitions` table to `Cell<HashMap>` —
      single-mutator + no re-entrancy makes the swap-pattern
      sound.)
- [x] `rg "Rc<RefCell<ObjectBody>>" crates-next/otter-vm/src` returns
      zero hits.
- [x] `rg "with_thread_default|enter_thread_default" crates-next/otter-vm/src/object.rs`
      returns zero hits.

## Closing

Tick 77A in [70-gc-master-tracker.md](./70-gc-master-tracker.md).
Hand the failing-callers list off to task 77B. Leave this file in
place until 77C closes — 77B authors need 77A's API decisions
visible.
