# Task 77 — Migrate `JsObject` to `Gc<ObjectBody>`

## Status

- [ ] `JsObject = Gc<ObjectBody>`
- [ ] `Rc<RefCell<ObjectBody>>` removed from `object.rs`
- [ ] `Traceable` impl traces shape keys + property values + prototype + named properties
- [ ] every `borrow()` / `borrow_mut()` on `ObjectBody` replaced
- [ ] object APIs take `RuntimeCx` / `NativeCtx` / `&mut GcHeap`
      explicitly; no thread-local heap lookup
- [ ] `gc_roots.rs` smoke tests for globals / module env / `this` un-ignored
- [ ] gates green

## Open work (post-76A)

Task 76A landed the structural pieces (`RuntimeCx<'rt>` / `NativeCtx<'rt>`
types, `!Send + !Sync` static assertions, compile-fail trybuild fixtures)
and audited the product surface. A minimal foundation is in place;
this task still needs:

- **API rewrite of `object.rs`** — every `JsObject` method that touches
  `ObjectBody` storage takes `&otter_gc::GcHeap` / `&mut otter_gc::GcHeap`
  (or `&NativeCtx<'_>` / `&mut NativeCtx<'_>` once the public binding
  surface lands). Today the methods still flow through
  `Rc<RefCell<ObjectBody>>` (pre-task-76 storage); the pre-WIP rewrite
  to `Gc<ObjectBody>` was reverted because the caller-side migration
  (~400 sites across ~29 files) does not fit a single session.
- **Caller migration** — every call site in `crates-next/otter-vm/src/`
  (lib.rs ~71 sites, object_statics.rs ~31, reflect.rs ~15, plus many
  smaller files) needs to thread `&self.gc_heap` / `&mut self.gc_heap`
  through the call chain. This is mechanical but bulky.
- **`Value::trace_value_slots` `Object` arm** — the WIP draft is
  preserved in the task spec; it lands in lockstep with the
  `Gc<ObjectBody>` migration above.
- **Un-ignore root smoke tests** — the `tests/gc_roots.rs::globals_*`
  / `module_env_*` cases stay `#[ignore]` until `Value::Object` walks
  through a real `Gc<ObjectBody>` slot.
- **Regression test** — `tests/gc_object_cycle.rs::proto_cycle_reaped`
  per the validation gate below.

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
   and call `self.inner.borrow()` now take explicit context. This is no
   longer optional after ADR-0005 / task 76A:
   - read paths take `&RuntimeCx` / `&GcHeap` as appropriate;
   - mutation paths take `&mut RuntimeCx` / `&mut GcHeap`;
   - write barriers run through that same context;
   - do not use `GcHeap::with_thread_default*` or any raw
     thread-local heap pointer.
   Method-style wrappers are acceptable only when the context is an
   explicit parameter, e.g. `obj.get(&mut cx, key)`.
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
- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm/src/object.rs crates-next/otter-vm/src` returns no object-path product-code hits.
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
