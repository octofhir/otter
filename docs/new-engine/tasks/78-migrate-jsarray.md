# Task 78 — Migrate `JsArray` to `Gc<ArrayBody>`

## Status

- [x] `JsArray = Gc<ArrayBody>`
- [x] `Rc<RefCell<ArrayBody>>` removed from `array.rs`
- [x] `Traceable` impl traces dense elements + named properties
- [x] all element / named-prop accesses go through `GcHeap`
- [x] all array APIs take explicit context; no thread-local heap lookup
- [x] `reserve_bytes` wired to `elements` capacity growth
- [x] gates green

## Goal

Same shape as task 77 for arrays. Adds the first off-slot accounting
case: dense `elements` vector capacity is the single biggest
unaccounted heap user in the legacy engine
(`PRODUCTION_READINESS_PLAN.md` §2.2). This task wires it through
`GcHeap::reserve_bytes` so the cap from task 73 actually catches
`Array.from({ length: 2**20 })`.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1,
§2.1 (caveat on legacy `tracked_bytes`), §10.1 R4, §10.2 Q5.

## Scope

1. **Type change** in `crates-next/otter-vm/src/array.rs`:
   ```rust
   pub type JsArray = Gc<ArrayBody>;
   pub struct ArrayBody {
       pub(crate) elements: SmallVec<[Value; 4]>,
       pub(crate) named_properties: Option<HashMap<String, Value>>,
   }
   impl Traceable for ArrayBody {
       const TYPE_TAG: u8 = …;
       fn trace(&self, v: &mut dyn FnMut(GcRaw)) {
           for el in &self.elements { trace_value(el, v); }
           if let Some(np) = &self.named_properties {
               for val in np.values() { trace_value(val, v); }
           }
       }
   }
   ```
2. **Off-slot accounting hook.** Wrap `elements` mutation through a
   helper that calls `heap.reserve_bytes(delta)` before grow and
   `heap.release_bytes(delta)` after shrink/free. The helper must be
   the *only* path that resizes `elements`; grep audit in PR. The
   helper receives `&mut RuntimeCx` / `&mut GcHeap` explicitly and must
   not resolve the heap through thread-local state.
3. **Trace the `Value` arm for `Array`** — one new `match` arm in
   `trace_value`.
4. **Update `Value::Array(JsArray)` clone semantics** — handle is
   `Copy`, no `Rc::clone` needed; this likely simplifies several
   sites in `array_prototype.rs`.

## Out of scope

- Sparse arrays (already filed as a follow-up in `array.rs:6`).
- TypedArrays / `ArrayBuffer` (`crates-next/otter-vm/src/binary/`)
  — separate, much larger task. File a follow-up if cap leaks
  observed there.

## Validation gates

- [x] No `Rc<RefCell<ArrayBody>>`.
- [x] `grep -R "with_thread_default\\|enter_thread_default\\|install_thread_default" crates-next/otter-vm/src/array* crates-next/otter-vm/src` returns no array-path product-code hits.
- [x] All existing engine fixtures pass.
- [x] New regression test `tests/gc_array_cap_kicks_in.rs`: configure
  cap = 4 MiB; run `Array.from({length: 1<<20})`; assert `Err(OtterError::OutOfMemory)`.
- [x] New regression test `tests/gc_array_self_reference.rs`:
  `let a = []; a.push(a); … drop; collect; assert no live Array`.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 78 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
