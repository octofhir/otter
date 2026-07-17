---
title: "Otter Macros"
---

Otter intrinsics, classes, and hosted modules are declared with a
family of **otter-themed** macros that live in `crates/otter-macros`.
Each macro corresponds to one role in the JS / module surface;
expansion produces ordinary Rust code plus a `BuiltinIntrinsic`-
shaped installer that bootstrap walks at startup. Generated code is
identical in shape to the hand-written installers under
[`crates/otter-vm/src/intrinsics/`](https://github.com/octofhir/otter/tree/main/crates/otter-vm/src/intrinsics)
— no new runtime path, no dynamic registration.

> **Status.** Otter-themed macros are the only supported surface;
> the legacy `#[js_namespace]` / `#[js_class]` / `#[js_fn]` /
> `#[js_constructor]` attribute macros have been removed. New code
> uses the otter-themed macros below.

## Naming Theme

Otters live in **holts**, gather in **couches**, float in **rafts**,
dig **burrows**, raise families in **lodges**, **dive** to forage,
grow a **pelt** for protection, and **groom** their fur. Each term
names exactly one macro role:

| Role                                           | Macro                | Mnemonic                                |
| ---------------------------------------------- | -------------------- | --------------------------------------- |
| Namespace intrinsic (non-constructible)        | `holt!`              | a den that holds methods + constants    |
| Class intrinsic (callable ctor + proto)        | `couch!`             | a couch of otters — ctor + instances    |
| Grouped method spec (table form)               | `raft!`              | a raft of methods floating together     |
| Single binding (annotates one Rust fn)         | `#[dive]`            | one focused act                         |
| Host-owned object surface                      | `burrow!`            | a private stash the embedder owns       |
| Hosted module loader (`otter:fs`, `node:url`)  | `lodge!`             | the family residence — module home      |
| `SafeTraceable` derive                         | `#[derive(Pelt)]`    | the coat that keeps roots alive         |
| `Finalize` derive (deferred)                   | `#[derive(Groom)]`   | the cleanup ritual                      |

The theme is in macro identifiers only. Generated diagnostics stay
spec-leaning — no "your raft is sinking" error messages.

## `holt!` — Namespace Intrinsic

A `holt!` declares a non-constructible namespace object — `Math`,
`JSON`, `Reflect`, `Atomics`, `Console`, `Symbol` (when used as a
namespace), `Temporal` (top-level), `Intl`. The macro emits a static
`NamespaceSpec`, a `BuiltinIntrinsic` adapter struct, and an
installer that calls into the existing `NamespaceBuilder`.

```rust,ignore
use otter_macros::{holt, raft};
use otter_vm::{NativeCtx, NativeError, Value};

holt! {
    name = "Math",
    feature = CORE,
    constants = [
        ("PI",  f64, std::f64::consts::PI,           read_only),
        ("E",   f64, std::f64::consts::E,            read_only),
        ("LN2", f64, std::f64::consts::LN_2,         read_only),
    ],
    methods = raft! {
        "abs"   / 1 => native_abs,
        "ceil"  / 1 => native_ceil,
        "floor" / 1 => native_floor,
        "pow"   / 2 => native_pow,
        "min"   / 2 => native_min,
        "max"   / 2 => native_max,
    },
}

fn native_abs(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // … spec body …
    Ok(Value::undefined())
}
// … one fn per `raft!` entry …
```

`feature` accepts `CORE`, `WEB`, `NODE_COMPAT`, etc. — the same
`BootstrapFeatures` flags the registry tests gate against.

## `couch!` — Class Intrinsic

A `couch!` declares a callable constructor plus its prototype and
its static method surface. Used for `Proxy`, `Date`, `Map`, `Set`,
`Promise`, `RegExp`, every `Temporal` class, every error class, and
every TypedArray.

```rust,ignore
use otter_macros::couch;

couch! {
    name = "Proxy",
    feature = CORE,
    constructor = (length = 2, call = proxy_ctor_call),
    statics = {
        "revocable" / 2 => proxy_revocable_call,
    },
    prototype = {
        methods = {
            // "name" / length => fn — inline rows
        },
        accessors = [
            // ("name", get = getter_fn, set = setter_fn)
        ],
    },
}
```

### Full surface

The following fields are all optional except `name`, `feature`, and
`constructor`:

- `constructor = (length = N, call = path[, callable_only = true][, is_abstract = true])`
  — `callable_only = true` drops the `[[Construct]]` slot so `new Foo(x)`
  throws "is not a constructor" via §10.1.10. Matches the
  §20.4.1 (`Symbol`), §21.1.1 (`Number`), §21.2.1 (`BigInt`),
  §20.3.1 (`Boolean`), §22.1.1 (`String`) shape. `is_abstract`
  documents intent for things like `%TypedArray%`; the install path
  is unchanged.
- `statics = { "name" / N => fn, ... }` — inline rows; auto-generates
  a `&[MethodSpec]` slice.
- `static_method_specs = [path::TO_SLICE, ...]` — references to
  pre-built `&[MethodSpec]` slices iterated through the
  constructor's `ObjectBuilder`. Used when the same slice is also
  consumed elsewhere (e.g. `Op::CallMethod` intrinsic dispatch).
  Inline `statics` and `static_method_specs` are independent — pick
  whichever fits, or use both.
- `static_constants = [("NAME", Kind(expr)[, attrs]), ...]` — pins
  numeric / boolean / nullish constants as own data properties on
  the constructor. `Kind` is one of `Undefined`, `Null`, `Boolean`,
  `Number`. Defaults to `Attr::read_only()` per §21.1.2.
- `prototype = { methods = { ... }, accessors = [...], method_specs = [...], parent = path }`
  — same dual (inline rows / slice ref) as the statics side, plus
  the optional `parent = path` override that replaces the default
  `%Object.prototype%` link. Used by per-kind TypedArrays that chain
  to `%TypedArray%.prototype`.
- `prototype_constants = [("NAME", Kind(expr) [, attrs]), ...]` —
  mirrors `static_constants` but pins on the prototype. Used for
  `TypedArray.prototype.BYTES_PER_ELEMENT` per §23.2.6.1.
- `ctor_parent = path` — resolver fn for the constructor's
  `[[Prototype]]` override. `path(global, heap) -> Value`. Used by
  per-kind TypedArrays to inherit from `%TypedArray%`.
- `install_on = path` — resolver fn for the parent host object the
  constructor binds on. `path(global, heap) -> JsObject`. Without
  it, the constructor binds on `globalThis`. Used for nested ctors
  (`Temporal.Instant`, `Temporal.Duration`).
- `post_install = path` — escape hatch. When set, the generated
  install body calls `path(heap, global, ctor)?` after pinning the
  constructor on `globalThis`. Used for things that don't fit
  declarative rows: setting hidden internal slots on the prototype
  (e.g. `[[BooleanData]] = false`), legacy accessors that need
  captures bound to the ctor identity (`RegExp.input` / `$_` /
  `$1`..`$9`), identity-shared globals (`Number.parseInt ===
  globalThis.parseInt`), or post-bootstrap installation of methods
  on `globalThis` that share the same plumbing.

Cross-class fixups that depend on the per-realm `WellKnownSymbols`
table (`@@toStringTag`, `@@iterator`, species accessors) do **not**
ride `couch!`; they stay in dedicated `install_<class>_well_knowns_post_bootstrap`
hooks that bootstrap calls after the symbol table is materialised.

For abstract constructors (e.g. `%TypedArray%`) the constructor body
throws `TypeError("not constructible directly")`. The macro syntax
is the same — `call` still points at a function; the function body
just throws.

## `raft!` — Grouped Method Spec

A `raft!` is the table form for method lists. Used inside
`holt!` / `couch!` body or standalone when assembling a method
table by hand.

```rust,ignore
let methods = raft! {
    "from"    / 1 => array_from,
    "of"      / 0 => array_of,
    "isArray" / 1 => array_is_array,
};
```

Per-entry attribute overrides land in 4.1a:

```rust,ignore
let methods = raft! {
    "name" / 0 => proxy_name attrs = enumerable_data,
    "from" / 1 => array_from,
};
```

## `#[dive]` — Single Binding

For one-off methods that don't fit a `raft!` table (long doc
comment, large body, or just stylistic preference), annotate the
Rust function directly. The enclosing `holt!` / `couch!` picks up
every `#[dive]`-annotated fn in the surrounding module and folds
them into its method list.

```rust,ignore
use otter_macros::dive;

#[dive(name = "fromEpochMilliseconds", length = 1)]
pub fn from_epoch_ms(ctx: &mut NativeCtx<'_>, args: &[Value])
    -> Result<Value, NativeError>
{
    // … spec body …
}
```

`#[dive]` on a top-level function (outside a `holt!` / `couch!`
block) is a doc / signature assertion only — it doesn't install
anything by itself.

## `burrow!` — Host-Owned Object

For embedder-side objects that aren't part of the JS standard
surface — CLI args, request-scoped state, web handler context. The
handle isn't owned by `bootstrap`; it's allocated against an
embedder root and exposed through the runtime API.

```rust,ignore
use otter_macros::{burrow, raft};

burrow! {
    name = "OtterRequestContext",
    fields = {
        url:     JsString,
        method:  JsString,
        headers: HostMap<JsString, JsString>,
    },
    methods = raft! {
        "header" / 1 => request_header,
    },
}
```

Burrow is the only macro that touches the embedder root contract;
see [Native Bindings](/otter/extensions/native-bindings/) for the
embedder side.

## `lodge!` — Hosted Module

Hosted modules served via `otter:` (built-in), `node:`
(compatibility shim), and user-defined prefixes. Each invocation
emits a scoped `pub fn install_<name>_module(scope, capabilities,
task_spawner) -> Result<RuntimeLocal<'scope>, RuntimeNativeError>`
installer plus a `pub static
<UPPER>_HOSTED_MODULE: HostedModule` row callers drop into their
`HOSTED_MODULES` array.

Plain exports — static `fn(ctx, args) -> Result<Value, NativeError>`
pointers installed through `RuntimeNativeScope::native_method`:

```rust,ignore
otter_macros::lodge! {
    prefix = "otter",
    name = "math",
    exports = {
        "add" / 2 => add_fn,
        "mul" / 2 => mul_fn,
    },
}
```

Capability-aware exports — each export takes a borrowed
`&CapabilitySet` snapshot captured at install time and is installed as a
rooted `RuntimeNativeScope::native_closure`:

```rust,ignore
otter_macros::lodge! {
    prefix = "otter",
    name = "kv",
    capabilities = true,
    exports = {
        "openKv" / 1 => open_kv,
        "kv"     / 1 => open_kv,
    },
}

fn open_kv(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    // … permission-checked open …
}
```

Optional fields `install = my_install_fn` and `module_static =
MY_MODULE` override the derived identifiers. See
[Hosted Modules](/otter/extensions/hosted-modules/) for the loader
contract.

## `#[derive(Pelt)]` — GC Body Tracing

`Pelt` derives `otter_gc::SafeTraceable` for GC body structs. Each
non-`#[pelt(skip)]` field is funneled through
`otter_vm::pelt::PeltField::pelt_trace`, which has blanket impls for
the leaf shapes every body actually uses:

- `Value` → calls `Value::trace_value_slots`.
- `Gc<T>` (and aliases such as `JsObject = Gc<ObjectBody>`,
  `UpvalueCell = Gc<UpvalueCellBody>`) → visitor receives the inline
  field address as `*mut RawGc`; `Gc::null()` is skipped so the
  derived body matches the hand-written `if !handle.is_null() { … }`
  guards byte-for-byte.
- `Option<T>` / `Vec<T>` / `[T; N]` / `Box<T>` / `RefCell<T>` →
  derive recurses into the inner `T` (each must implement
  `PeltField`).
- `bool` / `char` / `String` / every integer / `f32` / `f64` / `()`
  → no-op impls so the derive can call uniformly; tag these with
  `#[pelt(skip)]` only when you want to document intent.

Fields whose type does **not** implement `PeltField` (foreign
records, ICU formatter state, `BigInt`, `JsString` until it lands
on a GC body, custom enums) must carry `#[pelt(skip)]` — otherwise
the derive fails at compile time with the field span underlined.
This is the load-bearing safety property: silent omissions become
loud trait-bound errors.

`Cell<Value>` is intentionally **not** covered by a blanket impl:
visiting through `Cell::get()` would walk a value *copy*, not the
live cell slot, breaking relocation. Bodies with that shape stay
on a hand-written `SafeTraceable` impl until a safe escape hatch
(e.g. `#[pelt(via = path)]`) ships.

```rust,ignore
use otter_macros::Pelt;
use otter_vm::Value;

pub const PROXY_BODY_TYPE_TAG: u8 = 0x29;

#[derive(Debug, Pelt)]
#[pelt(tag = PROXY_BODY_TYPE_TAG)]
pub struct ProxyBodyGc {
    pub target:  Value,
    pub handler: Value,
    #[pelt(skip)] // primitive — no GC slot
    pub revoked: bool,
}
```

The `#[pelt(tag = <CONST>)]` attribute on the struct is required —
the macro never invents a tag. Reuse the per-body `<NAME>_TYPE_TAG`
const each hand-written installer already declares so tag
coordination stays centralised.

## `#[derive(Groom)]` — Finalize Hook (deferred)

`#[derive(Groom)]` is **not yet shipped**. `otter-gc` does not have a
per-body `Finalize` trait, and the sweep path has no place to
dispatch one — the existing
[`crates/otter-gc/src/finalize.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-gc/src/finalize.rs)
module owns weak / `FinalizationRegistry` bookkeeping; the generic
drop-time hook lives in [`otter_gc::SafeFinalize`]. `#[derive(Groom)]`
mirrors `Pelt`'s field-walk shape with a `#[groom(skip)]` opt-out
for fields managed by their own `Drop` impl. Bodies opt in by also
calling `heap.register_finalize::<MyBody>()` once at bootstrap.

## How the Macros Plug into Bootstrap

Every `holt!` / `couch!` / `lodge!` invocation emits a
`pub struct <Name>Intrinsic;` plus a `BuiltinIntrinsic` impl. The
caller adds one row to `BOOTSTRAP_ENTRIES` in
[`crates/otter-vm/src/bootstrap.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-vm/src/bootstrap.rs):

```rust,ignore
crate::bootstrap_entry!(crate::intrinsics::math::MathIntrinsic),
```

Bootstrap iterates the registry once at `Interpreter::new()`,
calling `install` on each entry in declaration order. The macros
do not register themselves — that stays an explicit, auditable
decision in `bootstrap.rs`.

## Invariants

- Exported JS names + arity are always explicit in macro metadata.
  The macro never infers them from Rust identifiers.
- Generated `MethodSpec` / `ConstructorSpec` values use the same
  spec types as the hand-written installers — the macros are pure
  code generation, not a new runtime path.
- Expansion compiles under `#![forbid(unsafe_code)]`. Any macro
  that needs `unsafe` for the expansion is a design bug.
- Every generated method call targets the current contract from
  [Native Call ABI](/otter/engine/native-call-abi/). The macro
  expansion fails to compile if the referenced function does
  not match that signature.

## See Also

- [Otter Macros — Design Note](/otter/macros/design/) — rationale, full
  surface, migration sequence, open questions.
- [Native Call ABI](/otter/engine/native-call-abi/) — the signature
  every generated method targets.
- [JS Surface Builders](/otter/extensions/js-surface-builders/) —
  the runtime helpers the macro expansions call into.
