# Otter Macros

Otter intrinsics, classes, and hosted modules are declared with a
family of **otter-themed** macros that live in `crates/otter-macros`.
Each macro corresponds to one role in the JS / module surface;
expansion produces ordinary Rust code plus a `BuiltinIntrinsic`-
shaped installer that bootstrap walks at startup. Generated code is
identical in shape to the hand-written installers under
[`crates/otter-vm/src/intrinsics/`](https://github.com/octofhir/otter/tree/main/crates/otter-vm/src/intrinsics)
— no new runtime path, no dynamic registration.

> **Status.** Otter macros are being introduced in Phase 4.1 of the
> architecture refactor. The legacy `#[js_namespace]` / `#[js_class]`
> attribute macros remain temporarily for backward compatibility and
> are deleted in sub-phase 4.1b once the otter-themed surface is
> fully populated. New code uses the otter-themed macros below.
> Refactor progress is tracked in
> [`docs/otter-macros-refactor-tracker.md`](https://github.com/octofhir/otter/blob/main/docs/otter-macros-refactor-tracker.md).

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
| `Finalize` derive                              | `#[derive(Groom)]`   | the cleanup ritual                      |

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
- `prototype = { methods = { ... }, accessors = [...], method_specs = [...] }`
  — same dual (inline rows / slice ref) as the statics side.
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
see [Native Bindings](../extensions/native-bindings.md) for the
embedder side.

## `lodge!` — Hosted Module

Hosted modules served via `otter:` (built-in), `node:`
(compatibility shim), and user-defined prefixes. The macro produces
the module descriptor (prefix, name, ESM export table, capability
metadata) plus the loader registration glue.

```rust,ignore
use otter_macros::{lodge, raft};

lodge! {
    prefix = "otter",
    name   = "kv",
    capabilities = [Net("kv.example.com")],
    exports = raft! {
        "get"  / 1 => kv_get,
        "set"  / 2 => kv_set,
        "open" / 1 => kv_open,
    },
}
```

Capability metadata is consulted by the loader **before** resolution,
so denied imports fail at resolve time, not at call time. See
[Hosted Modules](../extensions/hosted-modules.md) for the loader
contract.

## `#[derive(Pelt)]` — GC Body Tracing

`Pelt` derives `SafeTraceable` for GC body structs. The derive walks
fields:

- `Gc<T>` / `Value` → emits `slot.trace_value_slots(visitor)`
- `Option<Gc<T>>` → emits the conditional trace
- `[Gc<T>; N]` / `SmallVec<[Gc<T>; _]>` → emits per-element trace
- Plain `Copy` primitives / non-GC fields → skipped
- Foreign / unrecognised types → require `#[pelt(skip)]` or the
  derive fails at compile time with the field span underlined

```rust,ignore
use otter_macros::Pelt;

#[derive(Pelt)]
struct PromiseBody {
    fulfilled: Option<otter_gc::Gc<otter_vm::JsObject>>,
    rejected:  Option<otter_gc::Gc<otter_vm::JsObject>>,
    #[pelt(skip)] // primitive — no GC slot
    state: PromiseState,
}
```

## `#[derive(Groom)]` — Finalize Hook

`Groom` derives `Finalize` and walks the same field set. Fields
explicitly marked `#[groom(skip)]` are excluded from the finalize
walk. Use it on bodies that wrap external resources (file handles,
foreign-library handles, locked OS primitives) where Drop alone is
insufficient because finalization runs on a GC thread.

```rust,ignore
use otter_macros::{Pelt, Groom};

#[derive(Pelt, Groom)]
struct LibraryHandle {
    #[pelt(skip)] #[groom(skip)] // managed by libloading::Library Drop
    lib: std::sync::Arc<libloading::Library>,
}
```

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
- Every generated method call targets ABI v1 from
  [`docs/native-call-abi.md`](../../../native-call-abi.md). The
  macro expansion fails to compile if the referenced function does
  not match the v1 signature.

## See Also

- [Otter Macros — Design Note](../../../otter-macros-design.md) —
  rationale, full surface, migration sequence, open questions.
- [Refactor Tracker](../../../otter-macros-refactor-tracker.md) —
  live per-consumer port state.
- [Native Call ABI](../../../native-call-abi.md) — the signature
  every generated method targets.
- [JS Surface Builders](../extensions/js-surface-builders.md) —
  the runtime helpers the macro expansions call into.
