# Task 76 — Migrate `UpvalueCell` to `Gc<UpvalueCell>`

## Status

- [ ] `UpvalueCell = Gc<UpvalueCellBody>`
- [ ] `Rc<RefCell<Value>>` removed from upvalue path
- [ ] `Traceable` impl traces inner value
- [ ] closure capture / `MakeClosure` updated
- [ ] `gc_roots.rs` smoke test for upvalues un-ignored
- [ ] gates green

## Goal

Smallest-blast-radius migration. Closures are the canonical cycle
source (`function counter() { let n = 0; return () => ++n; }`'s inner
arrow holds the outer's locals). Migrating upvalues first validates
the `Gc<T>` + `Traceable` pattern on a leaf-shaped type before
touching `JsObject`.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1
(heap-shared types), §6.3 (eliminate `RefCell` from public path), §8
Phase 1 (migration order: upvalue first).

## Scope

1. **Replace** `pub struct UpvalueCell(Rc<RefCell<Value>>)` in
   `crates-next/otter-vm/src/lib.rs` with:
   ```rust
   pub type UpvalueCell = Gc<UpvalueCellBody>;
   pub struct UpvalueCellBody { value: Value }
   impl Traceable for UpvalueCellBody {
       const TYPE_TAG: u8 = …;
       fn trace(&self, v: &mut dyn FnMut(GcRaw)) { trace_value(&self.value, v); }
   }
   ```
   `trace_value` is a local helper that dispatches on `Value`
   variants and calls `v(handle.raw())` for each future-`Gc`-shaped
   variant. Today it is mostly empty (only upvalue is migrated); each
   subsequent migration task adds one arm.
2. **Update closure capture sites.** Every `Rc::new(RefCell::new(v))`
   becomes `heap.alloc(UpvalueCellBody { value: v })?`. Every
   `cell.borrow().clone()` becomes `heap.get(cell).value.clone()`.
   Every `*cell.borrow_mut() = v` becomes a sequence:
   ```text
   let slot_addr = heap.with_mut(cell, |b| {
       b.value = v;
       &raw mut b.value
   });
   if let Some(child) = v.as_gc_raw() {
       heap.write_barrier(cell.raw(), slot_addr, child);
   }
   ```
   The barrier insertion is **mandatory** — generational scavenger
   correctness depends on it ([architecture doc §5](../gc-architecture.md)).
3. **`MakeClosure` opcode handler** — capture path constructs
   `Rc<[UpvalueCell]>`; that array of handles is now an array of
   `Gc<UpvalueCellBody>` — still `Rc<[…]>` (the array spine is
   immutable post-construction; no GC needed for it in Phase 1).
4. **Un-ignore** the upvalue smoke test from task 75:
   `gc_roots.rs::upvalue_kept_alive_through_closure`.

## Out of scope

- Any other type. The `Value::Object`, `::Array`, etc. arms of
  `trace_value` stay empty until tasks 77+ land.
- `Rc<[UpvalueCell]>` → `Gc<[UpvalueCell]>` migration. Phase 1
  treats the spine as immutable shared state; Phase 3 may move it.

## Validation gates

- [ ] No `RefCell<Value>` remaining in `lib.rs`.
- [ ] No `Rc<RefCell<Value>>` in any closure-related code.
- [ ] `gc_roots.rs::upvalue_kept_alive_through_closure` passes.
- [ ] New regression test
  `tests/gc_upvalue_cycle.rs::counter_closure_no_leak`: build a
  closure that captures itself transitively, drop the outer ref,
  force GC, assert `heap_stats().by_type[UPVALUE_TAG].live_bytes ==
  0`.
- [ ] All existing engine fixtures still pass.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 76 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
