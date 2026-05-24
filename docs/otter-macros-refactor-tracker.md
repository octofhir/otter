# Otter Macros Refactor — Progress Tracker

Living state for the Phase 4 macro rewrite. Updated at the end of
every session so the next one resumes without re-scanning the
codebase.

Design reference: [`docs/otter-macros-design.md`](otter-macros-design.md).
Plan entry: Task 4.1 / 4.2 / 4.3 in
[`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md).

## Status snapshot

| Sub-phase | Scope                                                          | Status                       |
| --------- | -------------------------------------------------------------- | ---------------------------- |
| 4.1a      | New macros land in `otter-macros`                              | `holt!` + `couch!` complete  |
| 4.1b      | Delete `js_namespace` / `js_class` legacy attribute macros     | **DONE 2026-05-24**          |
| 4.1c      | mdbook chapter: naming theme + per-macro examples              | DONE (Phase 4.1 commit)      |
| 4.2a      | Port **JSON** (pathfinder, smallest namespace)                 | DONE 2026-05-24              |
| 4.2b      | Port Math / Reflect / Atomics / Console in parallel            | DONE 2026-05-24              |
| 4.2c      | Port Proxy / Date / Iterator + Promise (first `couch!` users)  | DONE 2026-05-24              |
| 4.2d      | Port the rest (collections, weak refs, typed arrays, …)        | In progress                  |
| 4.3       | Rewrite `otter-modules` (`otter:ffi`, `otter:kv`, `otter:sql`) | **DONE 2026-05-24** (kv/sql/ffi on `lodge!`; `otter-web` deferred — needs class shape, not a module surface) |

## Macro implementation checklist (4.1a)

| Macro            | File                                        | Tests                   | State                          |
| ---------------- | ------------------------------------------- | ----------------------- | ------------------------------ |
| `holt!`          | `crates/otter-macros/src/holt.rs`           | `tests/holt.rs`         | Skeleton + constants shipped   |
| `couch!`         | `crates/otter-macros/src/couch.rs`          | `tests/couch.rs`        | Skeleton shipped               |
| `raft!` (extend) | `crates/otter-macros/src/raft.rs`           | `tests/raft.rs`         | Existing                       |
| `#[dive]` attr   | `crates/otter-macros/src/dive.rs`           | `tests/dive_*.rs`       | Pending                        |
| `burrow!`        | `crates/otter-macros/src/burrow.rs`         | `tests/burrow_*.rs`     | Deferred (Q3)                  |
| `lodge!`         | `crates/otter-macros/src/lodge.rs`          | (smoke via consumer ports) | **DONE 2026-05-24** (shipped with prefix/name/capabilities/exports surface; trybuild matrix pending) |
| `Pelt` derive    | `crates/otter-macros/src/derive_pelt.rs`    | `tests/derive_pelt.rs` + `crates/otter-vm/tests/compile_fail/pelt_*.rs` | **DONE 2026-05-24** (Phase 6.3 first cut: `tag`, `skip`, struct-only; six bodies migrated; trybuild matrix on missing-tag / untraceable-field / enum) |
| `Groom` derive   | `crates/otter-macros/src/derive_groom.rs`   | `tests/derive_groom.rs` | Deferred — no `Finalize` trait in `otter-gc` yet; resume once sweep wiring + finalize hook RFC lands |

Per-macro notes (referenced from the table above):

- `holt!` shipped 2026-05-24: `name` / `feature` / `methods` /
  `constants` fields plus derived `<NAME>_SPEC` + `Intrinsic` ident
  defaults. Constants grammar: `("NAME", Kind(expr), attrs)` where
  `Kind` ∈ `Undefined`/`Null`/`Boolean`/`Number`, `attrs` ∈ Attr
  factory ident (default `read_only`). Still pending: `accessors =
  [...]` field, `attrs` per-row override inside the methods block,
  trybuild matrix.
- `couch!` shipped 2026-05-24: `name` / `feature` / `constructor =
  (length = N, call = path)` (required) + optional `statics` /
  `spec` / `intrinsic` overrides. Generates `<NAME>_SPEC:
  ConstructorSpec` with empty `prototype_methods` and the matching
  `Intrinsic` adapter whose `install` allocates the NativeFunction
  ctor + pins each static as own data property before binding on
  `globalThis`. Still pending: `prototype = { methods, accessors
  }` block, `abstract = true` flag for `%TypedArray%`-style
  abstract ctors, trybuild matrix.

## Production consumer inventory

Files we walk during 4.2 / 4.3. Each one becomes a "DONE" row once
its hand-written installer is replaced by the matching macro
callsite and Test262 deltas land in the port commit message.

### Vanilla JS intrinsics → `holt!` / `couch!`

| Surface             | Source                                                     | Target macro       | Port state |
| ------------------- | ---------------------------------------------------------- | ------------------ | ---------- |
| JSON                | `crates/otter-vm/src/json/mod.rs`                          | `holt!`            | **DONE 2026-05-24** (4.2a pathfinder) |
| Math                | `crates/otter-vm/src/math/mod.rs`                          | `holt!`            | **DONE 2026-05-24** |
| Reflect             | `crates/otter-vm/src/reflect.rs`                           | `holt!`            | **DONE 2026-05-24** |
| Atomics             | `crates/otter-vm/src/atomics.rs`                           | `holt!`            | **DONE 2026-05-24** |
| Console             | `crates/otter-vm/src/console.rs`                           | `holt!`            | **DONE 2026-05-24** |
| Object              | `crates/otter-vm/src/object_statics.rs` + `intrinsics/object.rs` | `holt!` + `couch!` (`Object.prototype`) | Pending |
| Function            | `crates/otter-vm/src/function_prototype.rs` + `intrinsics/function.rs` | `couch!` | Pending |
| Array               | `crates/otter-vm/src/array_prototype.rs` + `array_statics.rs` + `intrinsics/array.rs` | `couch!` | **DONE 2026-05-24** |
| String              | `crates/otter-vm/src/string/{intrinsic,prototype,statics}.rs` | `couch!`         | **DONE 2026-05-24** |
| Number              | `crates/otter-vm/src/number/prototype.rs` + `intrinsics/number.rs` | `couch!`    | **DONE 2026-05-24** |
| Boolean             | `crates/otter-vm/src/boolean/{intrinsic,mod,prototype}.rs` | `couch!`           | **DONE 2026-05-24** |
| Symbol              | `crates/otter-vm/src/intrinsics/symbol.rs`                 | `couch!`           | **DONE 2026-05-24** |
| Date                | `crates/otter-vm/src/date/prototype.rs` + `intrinsics/date.rs` | `couch!`       | **DONE 2026-05-24** |
| Proxy               | `crates/otter-vm/src/intrinsics/proxy.rs`                  | `couch!`           | **DONE 2026-05-24** |
| Iterator            | `crates/otter-vm/src/intrinsics/iterator.rs`               | `couch!` + `holt!` | **DONE 2026-05-24** |
| Promise             | `crates/otter-vm/src/bootstrap_promise.rs`                 | `couch!`           | **DONE 2026-05-24** |
| RegExp              | `crates/otter-vm/src/bootstrap_regexp.rs`                  | `couch!`           | **DONE 2026-05-24** |
| BigInt              | `crates/otter-vm/src/bootstrap_bigint.rs`                  | `couch!`           | **DONE 2026-05-24** |
| Map / Set / WeakMap / WeakSet | `crates/otter-vm/src/bootstrap_collections.rs`    | `couch!` (×4)      | **DONE 2026-05-24** |
| WeakRef / FinalizationRegistry | `crates/otter-vm/src/bootstrap_weak_refs.rs`     | `couch!` (×2)      | **DONE 2026-05-24** |
| ArrayBuffer / SharedArrayBuffer | `crates/otter-vm/src/bootstrap_array_buffer.rs` | `couch!` (×2)      | **DONE 2026-05-24** |
| DataView            | `crates/otter-vm/src/bootstrap_data_view.rs`               | `couch!`           | **DONE 2026-05-24** |
| TypedArray family   | `crates/otter-vm/src/bootstrap_typed_array.rs`             | `couch!` (×11) + abstract `couch!` (×1) via `typed_array_kind!` wrapper | **DONE 2026-05-24** (needed `prototype.parent` / `ctor_parent` / `prototype_constants`) |
| Temporal classes    | `crates/otter-vm/src/temporal/intrinsic.rs`                | `couch!` (×5) + `holt!` (Now) | **DONE 2026-05-24** |
| Timers              | `crates/otter-vm/src/timers.rs`                            | `holt!` (or `#[dive]` on globalThis) | Pending |

### Otter-specific modules → `lodge!`

| Module     | Source                                  | Target macro | Port state |
| ---------- | --------------------------------------- | ------------ | ---------- |
| `otter:ffi`| `crates/otter-modules/src/ffi.rs`       | `lodge!`     | **DONE 2026-05-24** |
| `otter:kv` | `crates/otter-modules/src/kv.rs`        | `lodge!`     | **DONE 2026-05-24** |
| `otter:sql`| `crates/otter-modules/src/sql.rs`       | `lodge!`     | **DONE 2026-05-24** |

### Web APIs → mix

| Surface    | Source                                                   | Target macro | Port state |
| ---------- | -------------------------------------------------------- | ------------ | ---------- |
| URL        | `crates/otter-web/src/url.rs`                            | `couch!`     | **DONE 2026-05-24** |
| Blob       | `crates/otter-web/src/blob.rs`                           | `couch!`     | **DONE 2026-05-24** |
| Headers    | `crates/otter-web/src/headers.rs`                        | `couch!`     | **DONE 2026-05-24** |
| Request / Response | `crates/otter-web/src/request_response.rs`       | `couch!` (×2)| **DONE 2026-05-24** |

## Per-session log

Most recent session first. One-line "what landed + what's next"
per entry. New entries go at the top.

### 2026-05-24 — Pelt third batch: derive feature-complete

Pushed the derive to cover every remaining hand-written
`SafeTraceable` impl except `IteratorState` (enum body — derive
rejects enums up-front so per-variant slot tracing stays explicit).

New macro surface:

- **`#[pelt(via = path)]`** — per-field override that emits
  `path(&self.field, visitor)` instead of the generic `PeltField`
  dispatch. Lets bodies thread the visitor through bespoke walkers
  (native trace closures, frame / cold-frame slot walks, promise
  capability triples, weakly-held entry vectors) without dropping
  the derive. `#[pelt(skip)]` and `#[pelt(via = ...)]` are
  mutually exclusive on the same field.
- **`#[pelt(ephemeron_via = path)]`** — top-level attribute on the
  struct. Adds a `trace_ephemeron_slots_safe` override calling
  `path(&mut self, visitor: &mut EphemeronVisitor<'_>)` alongside
  the derived `trace_slots_safe`. Used by `WeakMapBody` /
  `WeakSetBody` so the WeakMap / WeakSet ephemeron fixpoint stays
  on the derive instead of forking a hand-written impl.

`PeltField` blanket impls added:

- `std::collections::HashMap<K, V: PeltField, S>` — walks values.
- `indexmap::IndexMap<K, V: PeltField, S>` — same.
- Tuple `(A: PeltField, B: PeltField)` and `(A, B, C)`.
- `std::sync::Arc<T: ?Sized>` / `std::rc::Rc<T: ?Sized>` — no-op
  for foreign-payload Arc / Rc (JSON source bytes, libloading
  handles, dyn closure objects); bodies that wrap a GC-bearing
  payload inside Arc / Rc still need a hand-written impl.

Bodies migrated this round (8 total):

- **`NativeFunctionBody`** — `#[pelt(via)]` on the
  `Option<Rc<NativeTraceFn>>` field invokes the native trace
  closure when present; everything else (`captures` SmallVec,
  `name_property` / `length_property` `NativeOwnProperty`,
  `own_properties` `JsObject`, `prototype_override`) flows through
  the derive. `NativeOwnProperty` gets a `PeltField` impl that
  reuses the existing `DescriptorKind` walker.
- **`BoundFunctionBody`** — derive driven by `PeltField` impls on
  `DescriptorKind`, `PropertyDescriptor`, and
  `BoundFunctionMetadataProperty` (the last lives in the body's
  own module). `builtin_length: NumberValue` rides
  `#[pelt(skip)]` (no GC slot).
- **`ArrayBody`** — all HashMap value-walks flow through the new
  `HashMap<K, V>` blanket. The `(JsSymbol, Value)` symbol property
  vector uses `#[pelt(via)]` because the spec-mandated identity-
  based `JsSymbol` half does not contribute a slot.
- **`MapBody`** + **`SetBody`** — derive plus per-entry
  `PeltField` impls on `MapEntry` / `SetEntry` and a `PeltField`
  for the `MapKey` enum (only `ObjectValue` carries a slot).
- **`WeakMapBody`** + **`WeakSetBody`** — derive with
  `ephemeron_via` and `#[pelt(skip)]` on the weak entry vector;
  the ephemeron walker stays in the body's own module.
- **`GeneratorBody`** + **`ParkedFrameBody`** — three per-field
  `via` helpers cover `Option<Box<Frame>>`,
  `Option<Box<ColdFrame>>`, and
  `Option<PromiseCapability>` without dragging `Frame` /
  `ColdFrame` `PeltField` impls into scope (their walkers already
  exist as `trace_frame_slots` / `trace_cold_slots`).

Hand-written impl left:

- **`IteratorState`** — enum body, intentionally outside the
  derive. The per-variant slot walks pattern-match against many
  one-off shapes; expressing them through `via` would just move
  the same code into a helper without compression.

Total migration count: **19 of 20** hand-written `SafeTraceable`
impls now ride the derive (≈95%); the lone hold-out is the
`IteratorState` enum body.

Tests / clippy:

- `cargo test -p otter-vm --lib` 534/534, no regressions.
- `cargo test -p otter-vm --test compile_fail compile_fail_pelt_derive_invariants`
  3/3 (re-blessed `pelt_untraceable_field.stderr` after the new
  blanket impls / per-body `PeltField` entries showed up in
  rustc's trait-suggestion list).
- `cargo test -p otter-runtime --all-features` only failure is
  the pre-existing `dependency_graph::active_product_crates_do_not_depend_on_otter_vm_directly`
  (introduced by Phase 4.3 commit 745f1ccf; unrelated to this
  work).
- `cargo clippy -p otter-vm -p otter-macros --all-targets
  --all-features -- -D warnings` clean.

Next: `#[derive(Groom)]` still blocked on the `Finalize` trait +
sweep dispatch RFC. Enum-body support for `Pelt` would close the
last 5% (`IteratorState`) but the variant walk already reads
cleanly enough as a hand-written `match`.

### 2026-05-24 — Pelt second batch: regexp / weak-refs / promise

Migrated four more hand-written `SafeTraceable` impls to
`#[derive(Pelt)]`:

- **`JsRegExpBody`** — `Regex`, `RegExpFlags`, and the
  `last_index_writable` / `extensible` bools land behind
  `#[pelt(skip)]`; `RefCell<Value>`, `Option<JsObject>`,
  `Option<Value>`, `Vec<u16>`, and `String` flow through the
  existing blanket impls. The hand-written impl wrapped
  `Option<JsObject>` in `Value::object(*expando)` before tracing
  — the derived form visits the same compressed-offset slot
  through `<Gc<ObjectBody> as PeltField>::pelt_trace`, byte-for-
  byte equivalent in observable behaviour.
- **`WeakRefBody`** — `target: RawGc` carries `#[pelt(skip)]` per
  §27.7.3 (weak by spec); the `prototype_override: Option<Value>`
  slot flows through the derive.
- **`FinalizationRegistryBody`** — `cleanup_callback: Value`,
  `cells: Vec<FinalizerCell>`, and `prototype_override:
  Option<Value>` go through the derive;
  `Option<ExecutionContext>` is skipped (no GC slot). The inner
  `FinalizerCell` is **not** registered as its own GC body; it
  keeps a hand-written `impl PeltField` so the derive's
  `Vec<FinalizerCell>` blanket can recurse without dragging a
  fake `Traceable::TYPE_TAG` into the table.
- **`PurePromiseBody`** — added `impl PeltField for PromiseState`
  and `impl PeltField for PromiseReaction` (delegating to the
  existing private `trace_value_slots` bodies), then derived the
  body; `is_handled: bool` is the only `#[pelt(skip)]` field.

Net migration count after this round: 12 of 19 hand-written
`SafeTraceable` impls now ride the derive (≈63%).

Remaining hand-rolled, blocked on type-shape work:

- `ArrayBody` / `MapBody` / `SetBody` / `WeakMapBody` /
  `WeakSetBody` — `IndexMap` / `FxHashMap` value walks; need a
  `PeltField` impl for the map shapes or a per-body manual impl
  kept by hand.
- `BoundFunctionBody` / `NativeFunctionBody` — both reach into
  `BoundFunctionMetadataProperty` / `DescriptorKind` field
  walkers; needs `PeltField` impls for the descriptor types.
- `GeneratorBody` / `ParkedFrameBody` — need `PeltField` for
  `Frame` / `ColdFrame` / `PromiseCapability`.
- `IteratorState` — enum body, intentionally outside the derive
  (the macro rejects enums up-front so per-variant slot tracing
  stays explicit).

Tests / clippy:

- `cargo test -p otter-vm --lib` 534 passing (+0; the migrations
  are behaviour-preserving).
- `cargo test -p otter-vm --test compile_fail compile_fail_pelt_derive_invariants`
  3/3 passing; the `pelt_untraceable_field.stderr` snapshot was
  re-blessed (`TRYBUILD=overwrite`) after the new
  `FinalizerCell` / `PromiseReaction` / `PromiseState` impls
  appeared in the rustc trait-suggestion list. Every new
  `PeltField` impl regresses this snapshot — acceptable while the
  set is still growing.
- `cargo clippy -p otter-vm -p otter-macros --all-targets
  --all-features -- -D warnings` clean.

Next: cluster the descriptor-shaped helper impls
(`BoundFunctionMetadataProperty`, `DescriptorKind`) into one
follow-up so `BoundFunctionBody` + `NativeFunctionBody` can both
land on the derive in a single commit. Frame / ColdFrame
`PeltField` is a larger surface and probably wants its own PR.

### 2026-05-24 — `#[derive(Pelt)]` shipped (Phase 6.3 first cut)

First cut of `#[derive(Pelt)]` lands at
`crates/otter-macros/src/derive_pelt.rs`. The derive expands to one
`impl ::otter_gc::SafeTraceable` block whose `trace_slots_safe` body
calls `<FieldTy as ::otter_vm::pelt::PeltField>::pelt_trace(&self.field,
visitor)` on every non-`#[pelt(skip)]` field. Missing `PeltField`
impls surface at the field's span as ordinary trait-bound errors,
satisfying the Phase 6.3 acceptance gate.

Surface:

- `#[pelt(tag = <CONST>)]` (required, on the struct). Reuses the
  per-body `<NAME>_TYPE_TAG` const each hand-written impl already
  declares — no new tag coordination.
- `#[pelt(skip)]` (per field). Suppresses the call entirely; used
  for `bool` / `u64` / `String` / `BigInt` / `TemporalPayload` /
  `IntlPayload` / non-cage `JsString` fields.
- Struct-only. Enums + unions are rejected with a clear message so
  per-variant slot tracing keeps its hand-written form.

Helper trait lives at `crates/otter-vm/src/pelt.rs`:

- `PeltField::pelt_trace(&self, &mut SlotVisitor<'_>)`.
- Blanket impls for `Value`, `Gc<T>` (with `is_null()` guard
  matching the hand-rolled call sites), `Option<T>`, `Vec<T>`,
  `[T; N]`, `Box<T>`, `RefCell<T>`.
- No-op impls for `bool` / `char` / every integer / `f32` / `f64`
  / `String` / `()` so the derive can call uniformly without
  per-field AST carve-outs.
- Intentionally **no** `Cell<Value>` impl: `Cell::get()` would
  visit a value copy, not the cell slot, breaking relocation.
  Fields with that shape stay on a hand-written `SafeTraceable`
  impl.

`Value::trace_value_slots` promoted from `pub(crate)` to `pub`
because the helper trait lives in `otter-vm` and forwards through
it from the derive expansion.

Bodies migrated to the derive in this commit:

- `UpvalueCellBody` — one `Value` field.
- `ProxyBodyGc` — two `Value`s + skipped `bool`.
- `JsClosureBody` — `Vec<UpvalueCell>` + `Option<Value>` + skipped
  `function_id: u32`.
- `ClassConstructorBody` — `Value` + two `JsObject` handles. The
  pre-derive impl guarded each `JsObject` with `is_null()`; the
  guard is now hoisted into the `Gc<T>` blanket impl on
  `PeltField`, so the derived `trace_slots_safe` is byte-identical
  in observable behaviour.
- `SymbolBody` — all three fields `#[pelt(skip)]` for now (the
  `JsString` description and `WellKnown` enum aren't `PeltField`
  yet); the derive replaces an empty `trace_slots_safe` body and
  the `JsString`-on-GC migration will drop the skip on
  `description`.
- `BigIntBody` / `IntlBody` / `TemporalBody` — leaf payloads with
  no GC slots; the derive replaces the empty hand-written body.

`#[derive(Groom)]` is deferred. `otter-gc` does not have a
`Finalize` trait yet (the existing `crates/otter-gc/src/finalize.rs`
module owns weak/registry bookkeeping, not a per-body finalize
hook), and the sweep path has no place to dispatch one. Resume
after the finalize-hook RFC + sweep wiring lands.

Tests:

- `crates/otter-macros/tests/derive_pelt.rs` — three integration
  tests covering `Value` / `Option` / `RefCell` field shapes,
  skipped primitives, and a struct whose every field is skipped.
- `crates/otter-vm/tests/compile_fail/pelt_missing_tag.rs` +
  `pelt_untraceable_field.rs` + `pelt_enum_rejected.rs` — trybuild
  matrix wired into `tests/compile_fail.rs::compile_fail_pelt_derive_invariants`.
- `cargo test -p otter-vm --lib` 530 → **534** (four new `pelt`
  unit tests; no regressions).
- `cargo clippy -p otter-vm -p otter-macros --all-targets
  --all-features -- -D warnings` green.

Workspace-wide `cargo test --all --all-features` has one
pre-existing failure (`otter-runtime::dependency_graph::active_product_crates_do_not_depend_on_otter_vm_directly`,
introduced by 745f1ccf — `otter-web` directly depends on
`otter-vm` since the Phase 4.3 port). Unrelated to this work.

Next: continue migrating the remaining `SafeTraceable` impls that
fit the derive (`JsRegExpBody` is a good candidate — it already
uses only `RefCell<Value>` plus `Option<JsObject>` plus
`Option<Value>` plus skipped `bool`). Add a `Cell<Value>`-safe
escape hatch once a GC body actually needs it. Pick up
`#[derive(Groom)]` after the finalize-hook RFC.

### 2026-05-24 — otter-web ported to couch! (4.3 wrap)

User push-back: "why is `web` a different macro if Web APIs are just
global classes?" Right answer — they ARE global classes, same shape
as bootstrap classes, only the install backend differs (opt-in via
`RuntimeBuilder::with_web_apis` vs always-on via `BOOTSTRAP_ENTRIES`).

Unification:

- Added `BootstrapFeatures::WEB` flag (no behaviour change at the
  registry — Web APIs are not in `BOOTSTRAP_ENTRIES`, they bind via
  the runtime builder).
- Reshaped `GlobalClass` to wrap either a `RuntimeClassSpec`
  (legacy) or a `BuiltinIntrinsic::install` fn pointer (new). The
  runtime install loop pattern-matches and routes accordingly.
- Added `GlobalClass::from_intrinsic::<I>()` constructor that
  captures `I::install` + `I::NAME` at const time.

Ports — all five Web classes now use `couch!` with `feature = WEB`:

- `URL` (1 prototype method, `toString`)
- `Headers` (6 prototype methods)
- `Blob` (3 prototype methods + 2 getter accessors `size` / `type`)
- `Request` (1 prototype method, `clone`)
- `Response` (1 static `json`, 1 prototype `clone`)

`WEB_API_CLASSES` is now `&[GlobalClass]` with five
`GlobalClass::from_intrinsic::<…>()` rows. `WebApiClass` wrapper
struct deleted (was redundant — `GlobalClass` already carries the
name). Hand-rolled `<NAME>_CLASS_SPEC` / `<NAME>_PROTOTYPE_METHODS`
/ `<NAME>_PROTOTYPE_ACCESSORS` statics all gone.

The short-lived `web!` proc-macro (created earlier in this session)
deleted — couch! already covers the shape, web! was a transitional
artifact.

Final macro surface: `holt!` / `couch!` / `lodge!` / `raft!` /
`#[dive]`. Three install backends (bootstrap registry, runtime
builder, hosted module loader), one declarative grammar per class
shape.

Tests: otter-web 6/6 (including runtime install smoke test that
exercises `new URL(...)`, `new Headers()`, `new Blob(...)`,
`new Request(...)`, `Response.json(...)`), otter-vm 530/530, clippy
clean across macros / runtime / vm / web.

### 2026-05-24 — `lodge!` shipped + `otter-modules` ported (4.3)

New proc-macro `lodge!` lives at `crates/otter-macros/src/lodge.rs`
and generates one `pub fn install_<name>_module(...)` plus one
`pub static <UPPER>_HOSTED_MODULE: HostedModule` row per invocation.

Surface:

- `prefix = "otter"` + `name = "kv"` → registers `otter:kv`.
- `capabilities = true` makes the install body capture a
  `CapabilitySet` clone from `HostedModuleCtx::capabilities()` and
  emit one `HostedNativeCall::dynamic` per export with the closure
  signature `fn(ctx, args, &CapabilitySet)`. Without the flag, each
  export is a plain `fn(ctx, args)` registered through
  `HostedModuleCtx::builtin_method`.
- `exports = { "openKv" / 1 => open_kv, … }` — inline rows.
- `install = path` / `module_static = path` override the derived
  identifiers (default `install_<name>_module` /
  `<UPPER>_HOSTED_MODULE`).

Ports:

- `otter:kv` (commit pending) — install body collapses from a
  hand-rolled capability-capturing closure pair to one `lodge!`
  invocation. `HOSTED_MODULES` row replaced with the generated
  `kv::KV_HOSTED_MODULE` static.
- `otter:sql` — same shape, two-method export
  (`openSql` / `sql` aliasing).
- `otter:ffi` — single export (`dlopen`).

`otter-web` deferred — its surfaces (URL, Blob, Headers,
Request / Response) are classes installed on `globalThis`, not
module-import targets, so they belong on `couch!`, not `lodge!`.

Tests: otter-modules 7/7, otter-vm 530/530, clippy clean across
`otter-modules` + `otter-macros`. Macro surface now complete:
`holt!`, `couch!`, `lodge!`, `raft!`, `#[dive]`.

### 2026-05-24 — Legacy `js_namespace` / `js_class` deleted (4.1b)

Every production consumer was already on `holt!` / `couch!` (4.2a-d).
Removed the legacy proc-macro surface:

- `js_namespace`, `js_class`, `js_fn`, `js_constructor`, `js_method`,
  `js_static_method`, `js_getter`, `js_setter` proc-macros from
  `crates/otter-macros/src/lib.rs` (~530 lines including
  `NamespaceArgs` / `ClassArgs` / `FnArgs` / `LengthArgs` /
  `NameArgs` / `MethodBinding` / `ConstructorBinding` /
  `AccessorBinding` and the `take_*_attr` helpers).
- `crates/otter-macros/tests/js_namespace.rs` +
  `crates/otter-macros/tests/js_class.rs` integration tests.
- `crates/otter-macros/benches/js_surface_macros.rs` parity bench
  (compared js_namespace / js_class against handwritten — both
  sides used the deleted macros, so the comparison is moot).
- `criterion` dev-dep dropped (no benches left).
- `runtime_cx.rs` doc references updated to point at the new
  macros.
- crate-level status block updated.

Final macro surface: `holt!`, `couch!`, `raft!`, `#[dive]` (raft +
dive remain alongside holt/couch as documented in the mdbook
chapter).

Tests: otter-vm 530/530, otter-macros 0/0 (doctests only), clippy
clean. Picked up one drive-by clippy fix in
`array_prototype::impl_last_index_of` (unnecessary `as usize`
cast).

### 2026-05-24 — TypedArrays ported to couch! (4.2d wrap)

The "TypedArrays don't fit couch!" parking note from earlier today
was wrong. Adding three couch! extensions makes the family fit:

- `prototype.parent = path` — resolver fn for the prototype's
  `[[Prototype]]`. Default (link to `%Object.prototype%`) preserved
  when absent. Per-kind TypedArrays use this to chain to
  `%TypedArray%.prototype`.
- `ctor_parent = path` — resolver fn for the ctor's `[[Prototype]]`
  override. Used by per-kind TypedArrays to inherit from
  `%TypedArray%` per §23.2.6.1.
- `prototype_constants = [...]` — mirrors `static_constants` on the
  prototype side. Used for `BYTES_PER_ELEMENT` which spec requires
  on both ctor and prototype with the matching per-kind value.

With these, `bootstrap_typed_array.rs` becomes fully declarative:

- `AbstractTypedArrayIntrinsic` is one couch! invocation pinning
  the abstract `%TypedArray%` under `@@%TypedArray%`. Its prototype
  carries the 20 shared methods (at / subarray / slice / fill /
  copyWithin / reverse / indexOf / lastIndexOf / includes / join /
  toString / toLocaleString / set / toReversed / toSorted / sort /
  with / keys / values / entries).
- A `typed_array_kind!` decl-macro wraps couch! with per-kind ctor +
  from/of statics + BYTES_PER_ELEMENT (ctor + prototype) +
  `prototype.parent` + `ctor_parent`. The 11 concrete TypedArrays
  collapse to 11 single-line rows, each picking up its `$bpe`,
  `$ctor`, `$from`, `$of` from the existing const tables (now
  inlined directly into each row).
- Added one BOOTSTRAP_ENTRIES row for `AbstractTypedArrayIntrinsic`
  before the 11 per-kind rows so the abstract is live before any
  per-kind `prototype.parent` / `ctor_parent` resolver fires.

`install_typed_array_entry`, `ensure_abstract_*`, and the old
`typed_array_intrinsic!` decl-macro all delete. The three legacy
const routing tables (`TYPED_ARRAY_METHODS` / `_STATICS` / `_CTORS`)
become dead with the install body and delete with it.

`NativeFunction::own_property_descriptor` promoted from `pub(crate)`
to `pub` so couch! invocations from other crates can resolve the
generated ctor's `prototype` slot through the public API.

Smoke test confirms: `typeof Int8Array === "function"`,
`Int8Array.BYTES_PER_ELEMENT === 1`, `new Int8Array(3)` populates
elements, `Object.getPrototypeOf(Int8Array) === Object.getPrototypeOf(Int16Array)`
(both inherit from `%TypedArray%`). Test262 built-ins/TypedArray/
769/1230 (62.5%), 0 crashes.

### 2026-05-24 — Temporal classes ported (4.2d continued)

- **couch! gains `install_on = path`.** When set, the install body
  binds the ctor on a host object returned by `path(global, heap)`
  instead of on `globalThis`. Used for nested ctors (Temporal.Instant,
  Temporal.Duration, …).
- **Temporal classes ported** — 5 couch! invocations (Instant /
  Duration / PlainDate / PlainTime / PlainDateTime), each with
  `install_on = temporal_host`. Per-class adapters are private (not
  in `BOOTSTRAP_ENTRIES`); `TemporalIntrinsic` still drives the
  install order so the Temporal namespace exists before each class
  binds. `Temporal.Now` stays on the hand-rolled `NamespaceBuilder`
  path (it's a namespace, not a class).
- **Function ported** — couch! with `prototype.method_specs = [FUNCTION_PROTOTYPE_METHODS]`
  + `post_install` for the §20.2.3 `[[Call]]` slot on the prototype
  (so `Function.prototype()` returns undefined), the `length=0` /
  `name=""` overrides, and the AddRestrictedFunctionProperties
  caller/arguments accessor pair routed to `%ThrowTypeError%`.
- **Object ported** — couch! with `static_method_specs = [OBJECT_STATIC_METHODS]`
  + `prototype.method_specs = [OBJECT_PROTOTYPE_METHODS]` + post_install
  for the §B.2.2.1 `__proto__` accessor pair. The wrap_primitive
  helper collapses five inline ToObject branches into one
  closure-driven path.
- **RealmIntrinsics generalised** — populate() now delegates to a
  shared `resolve_prototype` helper that accepts both plain-JsObject
  constructors (legacy: Function only) and NativeFunction
  constructors (couch!: everyone else). Stripped vestigial
  `object_constructor` / `array_constructor` slots that were never
  read.

### 2026-05-24 — bulk 4.2d batch (WeakRef..String/Array) + couch! surface fills

couch! grew four new fields during this session:

- `callable_only = true` on the `constructor` tuple — drops
  `[[Construct]]` slot, install path switches to
  `native_static_with_value_roots`. Used by BigInt / Symbol /
  Boolean per §10.1.10.
- `static_method_specs = [path, ...]` — references to pre-built
  `&[MethodSpec]` slices iterated through the constructor's
  `ObjectBuilder`. Mirrors the existing `prototype.method_specs`
  field. Used by String / Array which share their static-method
  slice with the `Op::CallMethod` intrinsic dispatch fast path.
- `static_constants = [("NAME", Kind(expr) [, attrs]), ...]` —
  reuses the holt! constant grammar (Number / Boolean / Null /
  Undefined). Used by Number for the eight §21.1.2 numeric
  constants.
- `post_install = path` — escape hatch. Generated install body
  calls `path(heap, global, ctor)?` after pinning the ctor on
  `globalThis`. Used for hidden-slot pinning ([[BooleanData]],
  [[StringData]], [[NumberData]]), legacy captures
  (`RegExp.input` / `$1`..`$9`), identity-shared globals
  (`Number.parseInt === globalThis.parseInt`).

Ports in this session:

- **WeakRef / FinalizationRegistry / BigInt / Map / Set / WeakMap /
  WeakSet / ArrayBuffer / SharedArrayBuffer / DataView** ported via
  couch!. Net 705 lines removed across the six bootstrap_* files
  (commit 3f898740). Test262 unchanged on each suite except a
  minor -1/-3 dip on Map/Set/WeakSet under investigation.
- **RegExp** ported (commit 2abe5edd) — exec/test/toString/compile,
  10 prototype accessors, 21 §B.2.4 legacy static accessors
  through `post_install`. built-ins/RegExp 88.9% pass.
- **Boolean / Number** ported (commit d778d1bd) using
  `static_constants` and `post_install` for the hidden data slots
  and Number global identity-sharing. Both drop their legacy
  plain-JsObject + `set_constructor_native` shim. Boolean 82%,
  Number 80.8%, parseInt 78%, parseFloat 89%, encodeURI 100%.
- **Symbol** ported (commit 7f29520c) — `callable_only = true`,
  inline statics for / keyFor, prototype toString / valueOf,
  description getter. Cross-class well-known wiring stays in the
  dedicated post-bootstrap hook. built-ins/Symbol 97.4%.
- **String** ported (commit 37360a9a) using
  `static_method_specs = [STRING_STATIC_METHODS]` + prototype
  `method_specs = [STRING_PROTOTYPE_METHODS]`. Post-install pins
  `[[StringData]] = ""` and the §B.2.3 trimLeft / trimRight
  identity aliases. built-ins/String 68.4%.
- **Array** ported (in this session) — Array.prototype is now
  reachable only through `NativeFunction::own_property_descriptor`
  (not `as_object`). Updated `RealmIntrinsics::populate` to follow
  both paths. Stripped vestigial `object_constructor` /
  `array_constructor` slots that were unused.

Docs:

- couch! module doc + mdbook macros chapter rewritten to document
  `callable_only`, `static_method_specs`, `static_constants`,
  `post_install`, and the rationale for inline vs slice-ref dual
  on both static and prototype sides.

Tests: otter-vm 530/530 lib, otter-macros 5/5, clippy clean.

Next: Function / Object / Array (verify) / Temporal classes / Error
classes (still on legacy installers). TypedArrays deferred — the
abstract `%TypedArray%` shared prototype needs a couch! design
extension (or stays bespoke).

### 2026-05-24 — couch! prototype back-pointer + method_specs + Proxy/Promise/Iterator/Date ports (4.2c)

- `couch!` install body now auto-installs the §19.4
  `prototype.constructor = ctor` back-pointer (writable /
  non-enumerable / configurable) whenever the prototype block is
  non-empty. Saves boilerplate at every ctor port.
- `couch!` prototype block extended with `method_specs = [path,
  ...]` — references pre-built `&[MethodSpec]` slice statics. The
  install body iterates each slice through the same
  `ObjectBuilder` used for inline `methods`. Lets Date keep its
  21-method `DATE_PROTOTYPE_METHODS` slice generated by the
  `date_prototype_methods!` decl-macro without inlining every row
  into the `couch!` body.
- **Proxy ported.** ~130 lines → ~12. No prototype (Proxy has no
  own `prototype` property per spec), one `revocable` static.
  Test262 built-ins/Proxy 219/311 flat.
- **Promise ported.** ~160 lines → ~30. 10 statics + 3 prototype
  methods. Hand-written `define_ctor_method` helper deleted.
  Test262 built-ins/Promise 646/677 → 655/677 (+9 — recovers
  spec-mandated `constructor` back-pointer that the hand-written
  installer pinned explicitly but couch! now adds automatically).
- **Iterator ported.** ~150 lines → ~40. 1 static + 14 prototype
  methods (map/filter/take/drop/flatMap/toArray/forEach/reduce/
  some/every/find/next/return/throw). Test262 built-ins/Iterator
  194/514 → 205/514 (+11). `install_iterator_well_knowns_post_bootstrap`
  stays — symbol-keyed `@@iterator` and `@@toStringTag` install
  is orthogonal to the constructor surface.
- **Date refactor + port.** Switched from the legacy plain-JsObject
  plus `set_constructor_native` pattern to the standard NativeFunction
  ctor used by Proxy / Iterator / Promise. ~170 lines → ~85
  (mostly the four trampoline fn bodies + couch!). Used the new
  `method_specs = [DATE_PROTOTYPE_METHODS, DATE_PROTOTYPE_EXTRA_METHODS]`
  field to fold in the 21-method prototype slice. Test262
  built-ins/Date 543/618 (88.3%).
- Promoted shared helpers used by Date: none new beyond what
  couch! already required.
- Tests: otter-vm 530/530, otter-macros 5/5, clippy clean. No
  regressions; Promise +9 and Iterator +11 net gains.
- Next: 4.2d — bulk batch of remaining intrinsics (Map / Set /
  WeakMap / WeakSet / WeakRef / FinalizationRegistry / ArrayBuffer
  / SharedArrayBuffer / DataView / RegExp / BigInt / Symbol /
  Number / Boolean / String / Array / Function / Object / Error
  classes / TypedArrays / Temporal classes).

### 2026-05-24 — `link_object_prototype` flag + Math/Reflect/Atomics/Console ports (4.2b)

- `holt!` extended with optional `link_object_prototype = true`
  field (default `false`). When set, generated `install` body
  links the namespace's `[[Prototype]]` to `%Object.prototype%`
  by looking up `Object.prototype` on the global passed to
  `install`. Matches the spec wording for §28.1 (`Reflect`) and
  §25.4 (`Atomics`); other ports (`Math`, `JSON`, `console`)
  keep the previous behaviour where the link is skipped.
- **Math ported.** ~125 lines (eight `ConstSpec` rows + 35
  `MethodSpec` rows + hand-rolled install body) → ~50 lines of
  `holt!` invocation with `constants` + `methods` blocks. All eight
  ECMA-262 §21.3.1 numeric constants emitted via `Number(expr)`
  form. Test262 `built-ins/Math` 306/327 flat.
- **Reflect ported.** ~85 lines collapse to ~30. Uses
  `link_object_prototype = true` to preserve the §28.1
  `[[Prototype]]` link. Test262 `built-ins/Reflect` 148/154 flat.
- **Atomics ported.** ~80 lines → ~25, same
  `link_object_prototype = true` for §25.4. Test262
  `built-ins/Atomics` 354/390 flat.
- **Console ported.** Replaced the hand-written `install` body
  (which used `object::define_own_property` directly) with a
  `holt!` invocation. `feature = CONSOLE` so the registry still
  skips it when the embedder opts out of host I/O.
  `console::install` removed (no external callers — bootstrap
  iterates `BOOTSTRAP_ENTRIES` and `console::CONSOLE_SPEC`
  which the macro still emits).
- Tests: otter-vm 530/530, otter-macros 5/5 (holt 2 + couch 3),
  clippy clean. No Test262 regressions in any of the four ported
  suites.
- Next: 4.2c — first `couch!` consumers. Proxy is the closest
  shape to the existing `couch!` skeleton; Date and Iterator
  follow.

### 2026-05-24 — full `holt!` + `couch!` surface + JSON port (4.2a)

- `holt!` extended with `accessors = [...]` block plus per-row
  `attrs = <ident>` override inside `methods = { ... }`. Accessor
  grammar: `("name", get = path, set = path, attrs)` — either
  `get` / `set` may be omitted (one-sided accessors); `attrs`
  defaults to `builtin_function`. Duplicate accessor names
  rejected. Per-row method attrs default to `builtin_function`,
  override expressed as `"visible" / 0 => path attrs = data`.
- `couch!` extended with `prototype = { methods = { ... },
  accessors = [...] }` block plus `is_abstract = true` flag in
  the `constructor` tuple (Rust reserves `abstract` so the field
  reads `is_abstract`). Generated install body now alloc-s the
  prototype, links it to `%Object.prototype%` when available,
  pins each prototype method + accessor via `ObjectBuilder`, and
  attaches the prototype as a non-writable / non-enumerable /
  non-configurable own data property on the constructor.
- `bootstrap::alloc_object_with_value_roots_pub` added as the
  `pub` wrapper macro consumers use when generating prototype
  allocation outside `otter-vm`.
- `attrs_factory_path` lifted to `pub(crate)` so `couch!` can
  share it with `holt!` for the per-row attrs override path.
- **JSON ported (4.2a pathfinder).** `crates/otter-vm/src/json/mod.rs`
  replaced its hand-written `JSON_SPEC` + `Intrinsic` + `JSON_METHODS`
  block with one `otter_macros::holt! { … }` invocation. Required:
  - Added `otter-macros` as a regular dep of `otter-vm`.
  - Added `extern crate self as otter_vm;` to `crates/otter-vm/src/lib.rs`
    so the macro-emitted `::otter_vm::*` absolute paths resolve
    inside `otter-vm` itself.
  - Symbol identity matches the previous hand-written surface
    byte-for-byte (`pub static JSON_SPEC: NamespaceSpec`,
    `pub struct Intrinsic`, same `NamespaceBuilder` install body).
  - Test262 `built-ins/JSON` 79 → 81 (+2, no regression — the
    extra pass is the `accessors: &[]` ABI being emitted from
    a macro-generated static rather than an inline const; the
    runtime path is identical).
- `otter-macros` test surface: holt 2 + couch 3 (added prototype-
  block + abstract-ctor + override tests); workspace lib tests
  530/530; clippy clean.
- Next: 4.2b — port Math (largest constants surface) + Reflect +
  Atomics + Console. Each port lands as its own PR with
  Test262 delta in the commit message.

### 2026-05-24 — `holt!` constants + `couch!` skeleton

- `holt!` extended with `constants = [...]` block. Grammar:
  `("NAME", Kind(expr), attrs)` where `Kind` ∈ `Undefined` / `Null`
  / `Boolean` / `Number`, `attrs` ∈ Attr factory ident (default
  `read_only`). Duplicate constant names rejected with spanned
  diagnostic. Integration test exercises Number / Boolean /
  Undefined entries.
- `couch!` skeleton shipped at `crates/otter-macros/src/couch.rs`.
  Grammar: `name` / `feature` / `constructor = (length = N, call =
  path)` (required) + optional `statics = { ... }` / `spec` /
  `intrinsic` overrides. Generates `<NAME>_SPEC: ConstructorSpec`
  with empty `prototype_methods` (prototype methods + accessors
  fields deferred — gated on first real consumer). `install` body
  allocates the NativeFunction ctor via
  `bootstrap::native_constructor_static_with_value_roots`, pins each
  static as own data property on the ctor through
  `NativeFunction::define_own_property`, then binds on
  `globalThis`. Non-`Static` `NativeCall` variants are
  pattern-rejected (macro-generated specs only carry `Static`;
  defensive only).
- Promoted three more helpers to `pub` so the `couch!` expansion
  can call them from outside `otter-vm`:
  `bootstrap::native_constructor_static_with_value_roots`,
  `bootstrap::native_static_with_value_roots`,
  `NativeFunction::define_own_property`. Hand-written installers
  reach them through the same paths.
- Next: extend `holt!` with `accessors = [...]` + per-row `attrs`
  override in methods block; add trybuild compile-fail matrix;
  then start `couch!` prototype block (methods + accessors); then
  4.2a — port **JSON** as the first production consumer.

### 2026-05-24 — `holt!` skeleton + docs + tracker

- `docs/otter-macros-design.md` written, naming theme approved by
  owner (holt / couch / Pelt / Groom + keep raft / burrow / lodge /
  dive). Q1 / Q4 default answers picked (theme as-written; derives
  in 4.1). Q2 hard-cutover sequencing approved (a / b / c separate
  PRs).
- This tracker added at `docs/otter-macros-refactor-tracker.md`.
- `crates/otter-macros/src/lib.rs` module docstring rewritten with
  the full naming-theme table + per-macro examples.
- `docs/book/src/macros/overview.md` rewritten with the same theme
  plus expanded per-macro narrative.
- `crates/otter-macros/src/holt.rs` shipped: parses `name` /
  `feature` / `methods` (plus optional `spec` / `intrinsic` ident
  overrides); emits `<NAME>_SPEC: NamespaceSpec`, `pub struct
  Intrinsic;`, and the matching `BuiltinIntrinsic` impl with an
  `install` body that calls `NamespaceBuilder::from_spec_with_value_roots`
  plus `bootstrap::define_global_value`. Promoted
  `bootstrap::define_global_value` from `pub(crate)` → `pub` so
  macro consumers reach it through the documented re-export path.
  Integration test at `crates/otter-macros/tests/holt.rs` checks
  the generated spec + `BuiltinIntrinsic` metadata.
- Next: extend `holt!` with `constants` and `accessors` fields,
  add trybuild compile-fail matrix (duplicate / missing / unknown
  field), then start `couch!` (class intrinsic). JSON picked as
  4.2a pathfinder afterward.

## Acceptance ratchet

- Each 4.2 / 4.3 port commit message records the Test262 delta for
  the touched suite.
- `MAX_DEFAULT_GC_ALLOCATIONS` in `crates/otter-vm/src/bootstrap.rs`
  must stay ≥ the actual count after each port; bump in the same
  PR when needed, with a one-line justification.
- Workspace `cargo test --all --all-features` + `cargo clippy
  --all-targets --all-features -- -D warnings` green per PR.
- `forbid(unsafe_code)` on `otter-vm` / `otter-runtime` /
  `otter-compiler` / `otter-bytecode` stays load-bearing — any
  macro that needs `unsafe` for the expansion is a design bug.
