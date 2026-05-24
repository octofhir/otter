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
| 4.3       | Rewrite `otter-modules` (`otter:ffi`, `otter:kv`, `otter:sql`) | Pending                      |
|           | + `otter-web` if `burrow!` / `lodge!` apply                    |                              |

## Macro implementation checklist (4.1a)

| Macro            | File                                        | Tests                   | State                          |
| ---------------- | ------------------------------------------- | ----------------------- | ------------------------------ |
| `holt!`          | `crates/otter-macros/src/holt.rs`           | `tests/holt.rs`         | Skeleton + constants shipped   |
| `couch!`         | `crates/otter-macros/src/couch.rs`          | `tests/couch.rs`        | Skeleton shipped               |
| `raft!` (extend) | `crates/otter-macros/src/raft.rs`           | `tests/raft.rs`         | Existing                       |
| `#[dive]` attr   | `crates/otter-macros/src/dive.rs`           | `tests/dive_*.rs`       | Pending                        |
| `burrow!`        | `crates/otter-macros/src/burrow.rs`         | `tests/burrow_*.rs`     | Deferred (Q3)                  |
| `lodge!`         | `crates/otter-macros/src/lodge.rs`          | `tests/lodge_*.rs`      | Pending (4.3)                  |
| `Pelt` derive    | `crates/otter-macros/src/derive_pelt.rs`    | `tests/derive_pelt.rs`  | Pending                        |
| `Groom` derive   | `crates/otter-macros/src/derive_groom.rs`   | `tests/derive_groom.rs` | Pending                        |

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
| `otter:ffi`| `crates/otter-modules/src/ffi.rs`       | `lodge!`     | Pending (4.3) |
| `otter:kv` | `crates/otter-modules/src/kv.rs`        | `lodge!`     | Pending (4.3) |
| `otter:sql`| `crates/otter-modules/src/sql.rs`       | `lodge!`     | Pending (4.3) |

### Web APIs → mix

| Surface    | Source                                                   | Target macro | Port state |
| ---------- | -------------------------------------------------------- | ------------ | ---------- |
| URL        | `crates/otter-web/src/url.rs`                            | `couch!`     | Pending    |
| Blob       | `crates/otter-web/src/blob.rs`                           | `couch!`     | Pending    |
| Headers    | `crates/otter-web/src/headers.rs`                        | `couch!`     | Pending    |
| Request / Response | `crates/otter-web/src/request_response.rs`       | `couch!` (×2)| Pending    |

## Per-session log

Most recent session first. One-line "what landed + what's next"
per entry. New entries go at the top.

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
