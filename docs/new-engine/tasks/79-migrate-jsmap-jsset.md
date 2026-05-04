# Task 79 — Migrate `JsMap` and `JsSet` to `Gc<…Body>`

## Status

- [ ] `JsMap = Gc<MapBody>`
- [ ] `JsSet = Gc<SetBody>`
- [ ] `Rc<RefCell<…Body>>` removed from `collections.rs`
- [ ] `Traceable` impls trace keys + values
- [ ] Map / Set mutation APIs take explicit context for barriers and
      off-slot accounting
- [ ] gates green

## Goal

Routine migration. `Map` and `Set` are smaller surfaces than
`JsObject`; their bodies are `IndexMap<MapKey, Value>` /
`IndexMap<MapKey, ()>`. Both are cycle-prone (a Map holding itself as
a value, a Set containing an object that points back).

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1,
§8 Phase 1 step 3.

## Scope

1. **Type changes** in `crates-next/otter-vm/src/collections.rs`:
   ```rust
   pub type JsMap = Gc<MapBody>;
   pub struct MapBody { entries: IndexMap<MapKey, Value> }
   impl Traceable for MapBody {
       const TYPE_TAG: u8 = …;
       fn trace(&self, v: &mut dyn FnMut(GcRaw)) {
           for (k, val) in &self.entries {
               trace_map_key(k, v);
               trace_value(val, v);
           }
       }
   }
   ```
   Same shape for `SetBody`.
2. **`MapKey` trace** — `MapKey::Object(JsObject)` becomes `v(o.raw())`;
   primitive variants are leaves.
3. **`trace_value` extension** — arms for `Value::Map` and `Value::Set`.
4. **`reserve_bytes` hook** on `IndexMap` capacity changes (same
   pattern as task 78's array elements). Every mutation helper receives
   `&mut RuntimeCx` / `&mut GcHeap` explicitly; no thread-local heap
   lookup is allowed.

## Out of scope

- `WeakMap` / `WeakSet` — those need ephemeron handling, separate
  task (80).
- `Map.prototype.forEach` / iterator behaviour — already covered by
  existing tests; should not change semantically.

## Validation gates

- [ ] No `Rc<RefCell<MapBody>>` / `Rc<RefCell<SetBody>>`.
- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm/src/collections.rs` returns no Map / Set product-code hits.
- [ ] All existing engine fixtures pass.
- [ ] Regression test `tests/gc_map_self_value.rs`: `let m = new Map();
  m.set("k", m); … drop; collect; assert no live Map`.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 79 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
