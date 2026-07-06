---
title: "Otter Macros — Design Note"
---

This is the design note behind the otter-themed macro surface
(`holt!` / `couch!` / `raft!` / `burrow!` / `lodge!` / `#[dive]` /
`#[derive(Pelt)]` / `#[derive(Groom)]`). It captures the naming
rationale, the migration story away from the legacy
`#[js_namespace]` / `#[js_class]` macros, and the open questions
that remain.

The brief was generic `otter_intrinsic`, `otter_class`,
`otter_module`, `dive`, plus trace/finalize derives. This note
replaces the generic naming with the otter-themed surface
(`raft!` / `burrow!` / `lodge!` / `#[dive]`) and extends it to
cover the remaining roles. The point is keeping Otter recognisable
as Otter, not as "yet-another-engine".

## Goals

- Single canonical macro path per JS surface role; no parallel
  hand-written + macro-generated coexistence in `main`.
- Generated code lands on the Phase 2.5 native ABI verbatim — no
  ABI shim, no new runtime path.
- Span-correct diagnostics with `trybuild` regression tests for
  every invalid input shape we want to reject.
- `forbid(unsafe_code)` stays load-bearing in `otter-vm`; macro
  expansion must compile under that gate.
- Each macro generates the same `BuiltinIntrinsic` adapter shape
  the hand-written installers already produce so the bootstrap
  registry walks identical entries.

## Naming Convention

Otter terms only. No `js_*` prefix (deleted), no `otter_*` prefix
(redundant — the crate is already `otter_macros`). The vocabulary:

| Role                                              | Macro       | Why this term                                           |
| ------------------------------------------------- | ----------- | ------------------------------------------------------- |
| Namespace intrinsic (non-constructible)           | `holt!`     | otter holt = den; holds methods + constants, no spawn  |
| Class intrinsic (callable constructor + proto)    | `couch!`    | couch = group of otters on land — ctor + instances     |
| Group of bindings inside `holt!` / `couch!`       | `raft!`     | raft = family group in water; already in code          |
| Single binding (one method / accessor)            | `#[dive]`   | dive = single act; already an aspirational helper attr |
| Host-owned object surface (CLI args, host state)  | `burrow!`   | burrow = owned by one otter; not interpreter-spawn     |
| Hosted module loader (`otter:fs`, `otter:kv`, …)  | `lodge!`    | lodge = larger family residence; module-scope home     |
| `SafeTraceable` derive (what survives GC)         | `Pelt`      | pelt = outer coat that protects what's inside          |
| `Finalize` derive (drop cleanup)                  | `Groom`     | grooming = cleanup ritual                              |

`Pelt` and `Groom` are derive macros (`#[derive(Pelt)]`,
`#[derive(Groom)]`). Everything else is `macro_rules!` or proc
macro per the implementation note below.

Doc tone in generated diagnostics stays neutral / spec-leaning; the
otter theming is in macro identifiers only. We don't print
"your raft is sinking" on errors.

## Macro Surface

### `holt!` — namespace intrinsic

Replaces: hand-written `BuiltinIntrinsic` + `NamespaceSpec` for
Math, JSON, Reflect, Atomics, Console, Symbol (namespace surface),
Temporal (top-level namespace), Intl.

Shape:

```rust
holt! {
    name = "Math",
    feature = CORE,
    constants = [
        ("PI",  f64,  std::f64::consts::PI,     read_only),
        ("E",   f64,  std::f64::consts::E,      read_only),
        // …
    ],
    methods = {
        raft! {
            "abs"  / 1 => native_abs,
            "ceil" / 1 => native_ceil,
            "pow"  / 2 => native_pow,
            // …
        }
    },
}
```

Expansion:

- Static `MATH_SPEC: NamespaceSpec` with the listed methods,
  accessors, constants, and `Attr::global_binding()`.
- `pub struct MathIntrinsic;` implementing `BuiltinIntrinsic` with
  the matching `NAME` / `FEATURE` constants. `install` body calls
  `NamespaceBuilder::from_spec_with_value_roots(...).build()` and
  `bootstrap::define_global_value(...)`.
- Generated installer goes into `BOOTSTRAP_ENTRIES` via
  `bootstrap_entry!(crate::intrinsics::math::MathIntrinsic)`.
- The Rust functions (`native_abs`, …) are referenced by path; the
  macro never emits or rewrites their bodies. ABI v1 signature
  (`for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>`)
  is the only accepted shape; mismatches surface as ordinary type
  errors at the generated call site.

Accessors (less common) attach via `accessors = [...]` with
explicit getter / setter pairs.

### `couch!` — class intrinsic (constructor + prototype + statics)

Replaces: hand-written `js_class` plus the `NativeFunction`
constructor + `define_own_property` static-install pattern in
intrinsics/proxy.rs and the new Temporal class installer.

Shape:

```rust
couch! {
    name = "Proxy",
    feature = CORE,
    constructor = (length = 2, call = proxy_ctor_call, abstract = false),
    statics = raft! {
        "revocable" / 2 => proxy_revocable_call,
    },
    prototype = {
        methods = raft! { /* if any */ },
        accessors = [ /* if any */ ],
    },
}
```

`abstract = true` (e.g. `%TypedArray%`) emits a constructor body
that throws `TypeError("not constructible directly")`; the call
target is still required so the abstract ctor can produce
diagnostics with the right name.

Expansion mirrors `intrinsics/proxy.rs::install`: allocate the
`NativeFunction` constructor via
`native_constructor_static_with_value_roots`, define each static
as an own data property on the constructor, install on `global`
via `define_global`. Prototype methods get attached to the
`prototype` slot of the constructor through the same builder path.

### `raft!` — grouped method spec

Already exists; keep. Used inside `holt!` / `couch!` body to keep
method lists tabular instead of `vec![ MethodSpec { … }, … ]`.
Already supports per-entry `name / length => path`.

Possible extension: optional `attrs = builtin_function` /
`attrs = enumerable_data` per row when we need to override the
default `Attr::builtin_function()`.

### `#[dive]` — single binding attribute

For one-off methods that don't fit a `raft!` table (e.g. a method
with a long body and lots of doc comment), declare the JS name +
length inline on the Rust function:

```rust
#[dive(name = "from", length = 1)]
pub fn array_from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // …
}
```

Inside `holt!` / `couch!` the surrounding macro picks up
`#[dive(…)]`-annotated functions in the module body and folds them
into the method list automatically — the same way the current
`js_namespace` walks `#[js_fn(…)]`. Plain `#[dive]` on a top-level
function (outside a `holt!` block) just emits a doc note + asserts
the ABI signature; it doesn't install anything.

### `burrow!` — host-owned object

For embedder-side objects that aren't part of the JS standard
surface — CLI args, request-scoped state, web-handler context.
The handle isn't owned by `bootstrap`; it's allocated against an
embedder root and exposed via a runtime API.

Shape:

```rust
burrow! {
    name = "OtterRequestContext",
    fields = {
        url:    JsString,
        method: JsString,
        headers: HostMap<JsString, JsString>,
    },
    methods = raft! {
        "header" / 1 => request_header,
    },
}
```

Expansion emits a `BurrowHandle<OtterRequestContext>` type +
constructor function that the embedder pins on its own root set.
Methods receive `&mut NativeCtx<'_>` plus the burrow handle
unpacked from `this`.

Burrow is the only macro that touches the embedder root contract;
deserves its own runbook section in `docs/site/src/content/docs/engine/native-call-abi.md` when
the macro lands.

### `lodge!` — module install

Hosted modules served via `otter:` (built-in), `node:`
(compatibility shim), and user-defined prefixes. Generates the
module descriptor (prefix, name, ESM export table, capability
metadata) plus the loader registration glue.

Shape:

```rust
lodge! {
    prefix = "otter",
    name   = "kv",
    capabilities = [Net("kv.example.com")],
    exports = {
        "get"   / 1 => kv_get,
        "set"   / 2 => kv_set,
        "open"  / 1 => kv_open,
    },
    classes = [
        couch! {
            name = "KvHandle",
            // …
        }
    ],
}
```

Expansion produces a `LodgeDescriptor` const + a registration
function the runtime builder calls during module-loader setup.
Capability metadata is consulted by the loader before resolution,
so denied imports fail at resolve time, not at call time.

### `Pelt` derive — `SafeTraceable` body

Replaces hand-written `Traceable` impls on GC body structs. The
derive walks fields:

- `Gc<T>` / `Value` → emit `slot.trace_value_slots(visitor)`
- `Option<Gc<T>>` → emit conditional trace
- `[Gc<T>; N]` / `SmallVec<[Gc<T>; _]>` → emit per-element trace
- Plain `Copy` primitives / non-GC fields → skip
- Foreign types → require `#[pelt(skip)]` annotation or compile
  error pointing at the field

Compile-fail tests cover: untagged foreign type, wrong inner type
for `Option<…>`, missing `#[pelt(skip)]` annotation on a primitive
union with unclear safety.

### `Groom` derive — `Finalize` impl

Same shape, smaller surface. Emits the `Finalize::finalize` body
that calls per-field `Finalize` impls; fields explicitly marked
`#[groom(skip)]` are excluded.

## Macro Implementation Plan

Crate layout after 4.1:

```
crates/otter-macros/
  src/
    lib.rs              — re-exports
    holt.rs             — `holt!` proc macro
    couch.rs            — `couch!` proc macro
    raft.rs             — `raft!` proc macro (existing; extend with attrs override)
    dive.rs             — `#[dive]` helper attr
    burrow.rs           — `burrow!` proc macro
    lodge.rs            — `lodge!` proc macro
    derive_pelt.rs      — `Pelt` derive
    derive_groom.rs     — `Groom` derive
    common/
      ast.rs            — shared syn types (NameSpec, MethodEntry, …)
      diag.rs           — span helpers, error builders
  tests/
    holt_valid.rs       — happy paths
    holt_invalid.rs     — trybuild fail cases
    couch_valid.rs
    couch_invalid.rs
    raft_valid.rs
    burrow_valid.rs
    lodge_valid.rs
    derive_pelt.rs
    derive_groom.rs
```

Old proc macros (`js_namespace`, `js_class`, helper attrs
`js_fn`/`js_constructor`/`js_method`/`js_static_method`/
`js_getter`/`js_setter`) are deleted in the same PR that introduces
the new ones. Hard cutover; no compat re-exports.

## Migration Sequence

1. **Phase 4.1a** — land the new macros in `otter-macros` with
   trybuild coverage. No production callers yet. Tests-only PR
   on a short-lived branch.
2. **Phase 4.1b** — delete `js_namespace` / `js_class` / `raft`
   helper-attr legacy + their integration tests + the benches in
   `crates/otter-macros/benches/js_surface_macros.rs`. The bench
   crate gets new comparison cases against the otter macros in
   step 4.2.
3. **Phase 4.2a** — port one read-only intrinsic to `holt!` as a
   pathfinder: candidate is **JSON** (small surface, two methods,
   already on the static-spec install path). Land as its own PR;
   Test262 `built-ins/JSON` ratchets verify byte-for-byte
   compatibility.
4. **Phase 4.2b** — port **Math** + **Reflect** + **Atomics** in
   parallel after JSON proves the shape.
5. **Phase 4.2c** — port **Proxy** (first `couch!` user) +
   **Date** + **Iterator**.
6. **Phase 4.2d** — port the remaining intrinsics (`Object`,
   `Function`, `Symbol`, `Number`, `Array`, error classes,
   collections, weak refs, typed arrays, ArrayBuffer / DataView /
   SharedArrayBuffer, RegExp, Promise, Temporal classes).
7. **Phase 4.3** — `lodge!` rewrite of `otter-modules` (`otter:kv`,
   `otter:sql`, `otter:ffi`, Web APIs).

Each port lands on its own PR, with Test262 deltas for the
relevant suite in the commit message. The pre-existing manual
installers are deleted in the same PR — no parallel paths in
`main` between port-PRs.

## Trybuild Matrix

Compile-fail tests at minimum:

- `holt!` with duplicate `name` field
- `holt!` with missing `name`
- `holt!` with a method whose Rust function signature mismatches
  the ABI v1 native fn signature
- `couch!` with no `constructor`
- `couch!` with `abstract = true` and missing `call` body
- `raft!` with duplicate JS method name
- `#[dive]` on a function with the wrong signature
- `burrow!` field that isn't `Pelt`-able and isn't marked
  `#[pelt(skip)]`
- `lodge!` with conflicting prefix + name pair against a
  pre-registered module
- `#[derive(Pelt)]` on a struct with a non-Pelt field and no skip
  attribute

All trybuild outputs live under
`crates/otter-macros/tests/compile_fail/`; the test runner uses
the same `trybuild` machinery the rest of the workspace already
depends on (see `crates/otter-runtime/tests/compile_fail/`).

## Risks

1. **Span fragility.** Proc macros that mix attribute consumption
   with module-walk can drop spans on the helper attrs and surface
   errors at the wrong line. Mitigation: every emitted token gets
   an explicit `quote_spanned!` against the source ident; trybuild
   assertions pin the expected `^^^^^` underline range.
2. **ABI drift.** Generated code embeds the v1 ABI signature; an
   ABI v2 (if and when it ships) breaks every macro expansion at
once. Document this in `docs/site/src/content/docs/engine/native-call-abi.md` as a hard
   versioning rule: macros target the current ABI verbatim, no
   shim layer.
3. **`forbid(unsafe_code)`.** None of the planned expansions need
   unsafe — they wrap existing helpers from
   `intrinsics/shared.rs`. Guard with a `compile_fail!` test that
   asserts macro output is unsafe-free.
4. **Bootstrap allocation ratchet.** Each port changes the exact
   alloc order; `MAX_DEFAULT_GC_ALLOCATIONS` may need re-tuning.
   Acceptable as long as the new value is justified in the port
   commit message.
5. **Naming bus-factor.** Otter-themed names are charming but
   undocumented. Mitigation: this design note becomes the
   reference; macro doc comments cite it; the macro book chapter
under `docs/site/src/content/docs/macros/` lists every term with a
   one-sentence "what it does" gloss.

## Open Questions for Owner

- **Q1.** Approve the naming table? Specifically: comfortable with
  `holt` / `couch` / `pelt` / `groom` joining the existing
  `raft` / `burrow` / `lodge` / `dive` family? If any of these
  feel forced, alternates considered: `slide` (playground = group
  of methods), `bask` (resting = static surface), `paddle`
  (active = constructor). Recommendation: stick with the table as
  written — the four new terms map cleanly to four distinct roles
  with no overlap.
- **Q2.** Approve the hard-cutover sequencing? Specifically: 4.1a
  (macros land empty) + 4.1b (delete old) + 4.2a (JSON pathfinder
  ports), each as separate PRs on short-lived branches, or
  collapse 4.1a + 4.1b into one PR? Recommendation: keep them
  split — easier bisection if a macro bug surfaces during the JSON
  port.
- **Q3.** Burrow scope. Spec'd here as embedder-owned but the only
  current consumer is the test262 harness's `$262` agent surface.
  Defer `burrow!` until after the JS-standard ports (4.2a–d) ship
  so we have at least two real consumers to abstract over?
  Recommendation: defer.
- **Q4.** `Pelt` / `Groom` derives. Land alongside 4.1 or split
  into 6.3? Plan currently has 6.3 as "Derive Trace / Finalize"
  with 4.1 as a dependency. Keep them in 4.1 (so the GC body
  rewrites in 6.3 are mechanical) or push to 6.3 (so 4.1 ships
  earlier)? Recommendation: keep in 4.1 — derives are <300 lines
  per macro and the GC bodies will start using them immediately.

## Acceptance

Task 4.1 is DONE when:

- `crates/otter-macros/src/{holt,couch,raft,dive,burrow,lodge,derive_pelt,derive_groom}.rs`
  shipped with the surfaces above.
- Old `js_namespace` / `js_class` proc macros + helper attrs +
  integration tests deleted.
- Trybuild matrix above is green on the new macros.
- `cargo doc -p otter-macros` renders the doc comments with the
  examples in this note as `rust,ignore` blocks (real expansion
  tested via integration tests, not doctests, because the
  generated code references `otter_vm::*` paths that the macro
  crate can't depend on without a cycle).
- Plan doc 4.1 entry marked DONE with the commit-message-style
  delta block.

## Cross-references

- [Native Call ABI](/otter/engine/native-call-abi/) — ABI v1 the
  generated code targets.
- [`crates/otter-macros/src/lib.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-macros/src/lib.rs)
  — current proc-macro implementations.
- [`crates/otter-vm/src/intrinsics/`](https://github.com/octofhir/otter/tree/main/crates/otter-vm/src/intrinsics)
  — current hand-written installers; each becomes a macro callsite
  during Phase 4.2.
- [`crates/otter-vm/src/intrinsics/shared.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-vm/src/intrinsics/shared.rs)
  — runtime helpers the macro expansions call (no new runtime
  path).
