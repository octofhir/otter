# Task 77 — Migrate `JsObject` to `Gc<ObjectBody>`

## Status

- [ ] `JsObject = Gc<ObjectBody>`
- [ ] `Rc<RefCell<ObjectBody>>` removed from `object.rs`
- [ ] `Traceable` impl traces shape keys + property values + prototype + named properties
- [ ] every `borrow()` / `borrow_mut()` on `ObjectBody` replaced
- [ ] `gc_roots.rs` smoke tests for globals / module env / `this` un-ignored
- [ ] gates green

## Goal

The heart of the leak surface. `JsObject` is the heaviest hitter in
both allocation count and cycle frequency (prototype chains, method
self-references, `__proto__` cycles, accessor closures referencing
their own object). Migrating `JsObject` is what actually unblocks the
test sweep.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1,
§6.3 ("Eliminating `RefCell` from the public path"), §8 Phase 1
(migration order item 2).

## Scope

1. **Type change** in `crates-next/otter-vm/src/object.rs`:
   ```rust
   pub type JsObject = Gc<ObjectBody>;
   pub struct ObjectBody {
       pub(crate) shape: Rc<Shape>,                  // Shape stays Rc — immutable, shared
       pub(crate) slots: SmallVec<[PropertySlot; 8]>,
       pub(crate) prototype: Option<JsObject>,
       pub(crate) extensible: bool,
       // …
   }
   impl Traceable for ObjectBody {
       const TYPE_TAG: u8 = …;
       fn trace(&self, v: &mut dyn FnMut(GcRaw)) {
           if let Some(p) = self.prototype { v(p.raw()); }
           for slot in &self.slots { trace_property_slot(slot, v); }
           // shape.keys are interned strings (leaf, no trace)
       }
   }
   ```
2. **API change** — public methods on `JsObject` like `get`, `set`,
   `define_own_property`, `prototype`, etc. that today take `&self`
   and call `self.inner.borrow()` now need `&GcHeap` (read) or `&mut
   GcHeap` (mutate) threaded through. Two options:
   - **Free functions** that take `&mut GcHeap`. Cleanest; matches
     §6.3 of architecture doc.
   - **Methods on `ObjectBody`** taking `&mut self`, called via
     `heap.with_mut(obj, |body| body.set(…))`. Slightly more
     ergonomic when chained; same borrow discipline.
   Pick one and apply uniformly.
3. **Trace the `Value` arm for `Object`** — extend the `trace_value`
   helper from task 76 to dispatch on `Value::Object(o)` to `v(o.raw())`.
4. **Un-ignore** root smoke tests for globals, module env, and `this`
   slot.

## Hot-path considerations

- `Rc<Shape>` stays. Shapes are immutable post-transition; sharing
  via Arc/Rc is correct, GC handles only the *body*.
- Property-slot `Value` arms that contain `Gc<…>` get traced
  recursively when their children are migrated — the
  type-tag-dispatch trace fans out automatically.
- The `RefCell` removal eliminates one runtime borrow check per
  property access (~5 ns). Acceptable trade vs. one extra slot-table
  indirection.

## Out of scope

- `JsArray`, `JsMap`, `JsSet` — separate tasks (78, 79).
- Shape-key string interning migration to GC (Phase 5+ per §10.2 Q2).
- Extending the type-tag dispatch beyond `Object` + `UpvalueCell` —
  the rest land per migration task.

## Validation gates

- [ ] No `Rc<RefCell<ObjectBody>>` anywhere.
- [ ] All existing engine fixtures still pass.
- [ ] New regression test `tests/gc_object_cycle.rs::proto_cycle_reaped`:
  `let a = {}; let b = { __proto__: a }; a.__proto__ = b; … drop;
  collect; assert no live `Object`s except intrinsics`.
- [ ] `gc_roots.rs::globals_keep_object_alive` and
  `module_env_keeps_object_alive` un-ignored and passing.
- [ ] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 77 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
