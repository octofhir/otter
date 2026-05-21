# Phase 1.1 — `Value(u64)` cut-over plan

Companion to `docs/architecture-refactor-plan-2026-05.md` §Phase 1.

## Current state (after JsTemporal migration)

### Landed

- `crates/otter-vm/src/value/{mod,tag}.rs` — `#[repr(transparent)]
  pub struct Value(u64)`, `Copy`, default `undefined`. NaN-box layout:
  `TAG_INT32=0x7FF9`, `TAG_SPECIAL=0x7FFA`, `TAG_FUNCTION_ID=0x7FFB`,
  `TAG_PTR_OBJECT=0x7FFC`, `TAG_PTR_STRING=0x7FFD`,
  `TAG_PTR_FUNCTION=0x7FFE`, `TAG_PTR_OTHER=0x7FFF`. Doubles occupy
  every other 16-bit prefix; canonical NaN at `0x7FF8_0000_0000_0000`.
  Layout asserts: `size_of == 8 && align_of == 8`. Re-exported from
  the crate root as `TaggedValue` so it coexists with the legacy
  `pub enum Value`.
- `crates/otter-gc/src/compressed.rs` — `RawGc::header_type_tag()` +
  `RawGc::checked_cast<T: SafeTraceable>()`. The only `unsafe` blocks
  needed for tag-checked downcasts live here so `otter-vm` honours
  the workspace-wide `unsafe_code = forbid` policy.
- TaggedValue surface — constructors / accessors / predicates for:
  - Immediates: `undefined`, `null`, `hole`, `boolean`, `number_i32`,
    `number_f64`, `number(NumberValue)`, `function_id`.
  - Already-Gc-backed wrappers (legacy enum variant kept, but value
    layout already 4-byte `Gc<…>`): `object`, `array`, `map`, `set`,
    `weak_map`, `weak_set`, `weak_ref`, `finalization_registry`,
    `closure`, `bound_function`, `native_function`,
    `class_constructor`, `iterator`, `generator`, `regexp`, `promise`.
  - Additive GC body scaffolds (legacy `Rc`/`Arc` wrapper unchanged
    in some cases; the new `Gc<XxxBody>` handle is fully wired into
    the tagged surface): `string_gc`, `big_int_gc`, `symbol_gc`,
    `temporal_gc`, `intl_gc`, `proxy_gc`, `data_view_gc`,
    `typed_array_gc`, `local_array_buffer_gc`,
    `shared_array_buffer_gc`.
  - Coercions decidable without heap access: `to_boolean_pure`,
    `typeof_pure`.
  - Family-kind dispatch enums for single-call `match`:
    `ObjectFamilyKind`, `FunctionFamilyKind`, `OtherFamilyKind`.
- Tag collisions resolved: `ITERATOR_STATE_TYPE_TAG` bumped
  `0x1c → 0x24` to free `0x1c` for `BOUND_FUNCTION_BODY_TYPE_TAG`.
- **JsIntl wrapper migration complete** (commit `f9939ac3`).
  `JsIntl { inner: Rc<IntlPayload> }` is gone; the wrapper now holds
  `IntlHandle + cached IntlKind`. `with_payload<F,R>`,
  `payload_clone`, `kind`, `ptr_eq` all flow through the GC handle.
  Every `require_<X>` helper in `intl/*.rs` returns owned variants;
  `dispatch::construct` takes `&mut GcHeap` and surfaces
  `OutOfMemory`. Pattern template for the remaining wrappers.
- **JsTemporal wrapper migration complete.**
  `JsTemporal { inner: Rc<TemporalPayload> }` is gone; the wrapper now
  holds `TemporalHandle + cached TemporalKind`. All
  `temporal/*.rs` `parse_*_arg` / `expect_*` / `duration_arg` /
  `require_*` helpers thread `&GcHeap` / `&mut GcHeap` explicitly;
  `make_temporal(args, payload)` allocates via `IntrinsicArgs::gc_heap`,
  `alloc_temporal_value(heap, payload)` covers the static-dispatch
  path. `temporal::call_static` is `&mut GcHeap`; `load_property`
  takes `&GcHeap`. `Value::trace_value_slots` visits the embedded
  `TemporalHandle` slot via `JsTemporal::trace_value_slots`.
- **JsSymbol wrapper migration complete.**
  `JsSymbol { body: Rc<SymbolBody> }` is gone; the wrapper now holds
  `SymbolHandle` plus a cached `description` / `well_known_tag` /
  `registered` triple so the hot accessors (`description()`,
  `well_known_tag()`, `is_registered()`, `descriptive_string()`,
  `identity_addr()`) stay heap-free. `JsSymbol::new` /
  `JsSymbol::well_known` / `JsSymbol::registered` allocate via
  `alloc_symbol(heap, …)` and surface `OutOfMemory`.
  `WellKnownSymbols::new(string_heap, &mut gc_heap)` surfaces a folded
  `WellKnownInitError`; init order in `Interpreter::with_string_heap_cap`
  now constructs `GcHeap` before the well-known table.
  `SymbolRegistry::for_key(&mut gc_heap, &string_heap, key)` allocates
  the registered body and returns `SymbolRegistryError`. Single
  `Interpreter::symbol_for_key(&mut self, key)` helper splits the
  registry / heap borrows for native call sites.
  `Op::SymbolCall` dispatch now takes `&mut Interpreter` so
  `construct_symbol` can allocate. `GcTrace` impls for `JsSymbol`,
  `WellKnownSymbols`, `SymbolRegistry` walk every embedded handle so
  registry / table singletons survive collection.
  `Value::trace_value_slots` gains the `Value::Symbol` arm via
  `JsSymbol::trace_value_slots`. `MapKey::Symbol` hashing routes
  through `identity_addr() -> usize` (the handle's compressed offset).

### Type tag map (current)

```
0x10 UpvalueCell          0x18 FinReg               0x20 JsStringBody
0x11 ObjectBody           0x19 PurePromise          0x21 StringChunk
0x12 ArrayBody            0x1a Generator            0x22 ShapeBody
0x13 MapBody              0x1b ParkedFrame          0x23 JsClosureBody
0x14 SetBody              0x1c BoundFunctionBody    0x24 IteratorState
0x15 WeakMapBody          0x1d NativeFunctionBody   0x25 BigIntBody
0x16 WeakSetBody          0x1e JsRegExpBody         0x26 SymbolBody
0x17 WeakRefBody          0x1f ClassConstructorBody 0x27 TemporalBody
                                                    0x28 IntlBody
                                                    0x29 ProxyBodyGc
                                                    0x2a DataViewBodyGc
                                                    0x2b TypedArrayBodyGc
                                                    0x2c LocalArrayBufferBodyGc
                                                    0x2d SharedArrayBufferBodyGc
```

Next free: `0x2e`.

### Test surface

- 18 unit tests in `crates/otter-vm/src/value/tests`.
- 3 unit tests in `crates/otter-vm/src/bigint/gc_body/tests`.
- 3 unit tests in `crates/otter-vm/src/closure/tests`.
- 47 `gc_invariants` tests (root tracing, weak-ref + finalization,
  array cycles, etc.) stay green.
- Total: 525 lib tests in `otter-vm` pass. Workspace `cargo check`
  is green end-to-end.

## Remaining work — wrapper migrations

Each wrapper still has a legacy `Rc<…>` / `Arc<…>` inner. The
TaggedValue surface bypasses the wrapper via `*_gc` constructors that
take the new `Gc<XxxBody>` handle, but the legacy enum variant
`Value::Intl(JsIntl)` / `Value::Symbol(JsSymbol)` / etc. still flows
through the old wrapper. The wrapper migration replaces the inner
storage with the GC handle and updates every call site that reads
the payload or constructs the wrapper.

Ordered by call-site count, smallest first:

| Wrapper           | Inner today                                     | Sites (~) | GC body ready | Notes |
|-------------------|-------------------------------------------------|-----------|---------------|-------|
| `JsIntl`          | done — `f9939ac3`                               | —         | —             | template |
| `JsTemporal`      | done — `23d7e85c`                               | —         | —             | dispatch + load_property + helpers `&mut GcHeap` plumbed |
| `JsSymbol`        | done — this commit                              | —         | —             | WellKnownSymbols/SymbolRegistry `&mut GcHeap`; root scan walks registry + table; SymbolCall dispatch `&mut Interpreter` |
| `BigIntValue`     | done — this commit                              | —         | —             | pure 4-byte `Gc<BigIntBody>`, no Rc cache; `with_inner` / `clone_inner` / `to_decimal_string(heap)` / `sign(heap)` / `is_zero(heap)` / `numeric_eq(other, heap)`; `PartialEq` = handle `ptr_eq`; `abstract_ops::same_value(heap)` routes spec-numeric BigInt eq; ~50 files threaded `&GcHeap`/`&mut GcHeap` (object/descriptor_core, binary/*, json/*, conversion, collections, intrinsics, lib.rs Value::{to_boolean,display_string}). MapKey::BigInt hashes by handle offset. otter-runtime + otter-node + otter-web + otter-modules signatures updated; `pub use otter_gc` re-exports added |
| `JsProxy`         | `Rc<ProxyBody>` + `Cell<bool> revoked`           | ~80       | `ProxyBodyGc` | `.target() / .handler() / .is_revoked() / .revoke()` all gain heap params; revoke becomes `with_payload` mutation |
| `JsDataView`      | `Rc<DataViewBody>`                              | TBD       | `DataViewBodyGc` | thin wrapper; few read sites |
| `JsTypedArray`    | `Rc<TypedArrayBody>` + `Cell<expando>`          | TBD       | `TypedArrayBodyGc` | expando lazy-init becomes `with_payload` |
| `JsArrayBuffer`   | `BufferStorage::{Local(Rc), Shared(Arc)}`       | TBD       | two bodies   | shared keeps `Arc<SharedBody>` (cross-thread); local moves bytes into GC body |
| `JsString`        | `Arc<StringRepr>`                               | ~580      | `JsStringBody` (already chunked GC body exists) | biggest; cons/sliced/thin reprs need GC bodies too; rope ops need recurse-through-GC |

### Wrapper migration template (from JsIntl)

1. Reshape the wrapper struct:
   - `inner: Rc<Body>` → `inner: Handle` (= `Gc<Body>`).
   - Add a cached lightweight discriminator (`kind`, `len`, …) if
     the legacy wrapper exposes one without a heap touch.
2. Constructor: `Wrapper::new(payload)` → `Wrapper::new(heap: &mut
   GcHeap, payload) -> Result<Self, OutOfMemory>`.
3. Reader API:
   - `with_payload<F, R>(self, heap, f)` — closure-style read.
   - `payload_clone(self, heap)` — owned clone for callers that
     need to return across a borrow boundary.
   - Drop `payload() -> &Payload`; callers can't keep refs into
     GC bodies without explicit borrow scopes.
4. Identity: `ptr_eq(self, other) -> bool` via `Gc::eq`.
5. Drop `use std::rc::Rc;` if no longer needed.
6. Update every `Wrapper::new` call site to pass `&mut GcHeap` and
   `?` the error.
7. Update every `wrapper.payload()` site:
   - If used inside a single match arm: call `wrapper.with_payload(
     heap, |p| match p { … })`.
   - If used across borrow boundaries (e.g. `require_X` helpers
     returning `&'a XPayload`): change to owned via `payload_clone`,
     then add `&` borrows at downstream callers.
8. Update every `wrapper.ptr_eq(other)` site to dereference the
   handle borrow if needed (`a.ptr_eq(*b)`).
9. If the wrapper's display string uses payload introspection
   without heap (e.g. `i.kind().class_name()`), use the cached
   discriminator on the wrapper itself.

## Next-step priority

Recommended order:

1. **`BigIntValue`** — `.as_inner()` returns `&BigInt` from ~71
   sites; need to introduce a `with_inner<F, R>` closure helper
   and convert each call site. `BigInt::clone()` is too expensive
   for hot arithmetic, so `with_inner` is the only viable path.
2. **`JsDataView` / `JsTypedArray` / `JsArrayBuffer`** — all
   interrelated. ArrayBuffer migrates first (Local body to GC,
   Shared keeps Arc inside a GC body). DataView + TypedArray then
   migrate to hold the new buffer handles.
3. **`JsProxy`** — `.target() / .handler() / .is_revoked() /
   .revoke()` × 80 sites. Revoke becomes `&mut GcHeap` via
   `with_payload`. Touches `object_internal_ops.rs` extensively.
4. **`JsString`** — biggest scope (~580 sites). Cons / sliced /
   thin string reprs all need GC bodies; rope concatenation must
   re-enter the GC heap; the string heap accountant must thread
   through GC alloc. Plan for a separate session series.

## Final cut-over (after all wrappers migrate)

1. Delete the legacy `pub enum Value` body in
   `crates/otter-vm/src/lib.rs:217-401`.
2. Replace with `pub use value::Value;`.
3. Delete the `pub use value::Value as TaggedValue;` alias.
4. Sweep ~6 500 `Value::Variant(…)` pattern-match sites:
   - Constructors: `Value::Object(o)` → `Value::object(o)`.
   - Predicates inside guards: `matches!(v, Value::Array(_))` →
     `v.is_array()`.
   - Pattern bodies: `Value::Closure { function_id, … }` →
     `Value::Closure(c) => heap.read_payload(c, |b| …)`.
   - Coarse dispatch: `match v { Value::Array(_) => …
     Value::Map(_) => … }` → `match v.object_family_kind() {
     Some(ObjectFamilyKind::Array) => … }`.
5. Add `static_assertions::assert_impl_all!(Value: Copy);` once
   the cut-over completes.
6. Re-run Test262 baseline + microbenchmarks; gate merge on
   no-regression.

## Sanity invariants

- `size_of::<Value>() == 8` and `align_of::<Value>() == 8` —
  already enforced via const-asserts.
- All GC bodies' `SafeTraceable::TYPE_TAG` constants live in
  the `0x10..=0x2d` window; next free is `0x2e`.
- Tag-checked downcasts go through `RawGc::checked_cast`
  exclusively. No inline `unsafe { Gc::from_offset(…) }` in
  `otter-vm`.
- No `Cell`/`RefCell` inside GC bodies. Mutators flip fields via
  `heap.with_payload`.
