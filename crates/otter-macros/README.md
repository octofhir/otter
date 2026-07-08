# otter-macros

Zero-cost JavaScript / module surface macros for Otter. Each macro maps to one
role in the JS / module surface and expands to ordinary Rust: static spec data
(`NamespaceSpec` / `ClassSpec` / `ConstructorSpec` / `MethodSpec`) plus a
`BuiltinIntrinsic`-shaped installer that bootstrap walks at startup. There is no
new runtime path and no dynamic registration.

The full per-macro reference lives in the crate rustdoc (`cargo doc -p
otter-macros --open`) — this file is the narrative entry point.

## Macro family

| Role                                     | Macro              | Mnemonic                         |
|------------------------------------------|--------------------|----------------------------------|
| Namespace intrinsic (non-constructible)  | `holt!`            | a den holding methods + constants |
| Class intrinsic (callable ctor + proto)  | `couch!`           | a couch of otters — ctor + instances |
| Grouped method-spec table                | `raft!`            | a raft of methods floating together |
| Single binding (annotates one Rust fn)   | `#[dive]`          | one focused act                  |
| Host-owned object surface                | `burrow!`          | a private stash the embedder owns |
| Hosted module loader (`otter:fs`, …)     | `lodge!`           | the module home                  |
| `SafeTraceable` derive (GC body fields)  | `#[derive(Pelt)]`  | the coat that keeps roots alive  |
| `Finalize` derive (drop-time cleanup)    | `#[derive(Groom)]` | the cleanup ritual               |

Exported JavaScript names and arity are always explicit in the macro metadata —
the macro never infers them from Rust identifiers.

## Building values in method bodies

The macros describe *surface shape*; the method **body** you write is a plain
native function:

```rust
fn my_method(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError>
```

Inside that body, the Otter young generation is a **moving** collector: any
allocation can relocate any young object, so a `Value` / `JsObject` / `JsString`
held in a Rust local **goes stale the moment a later allocation triggers a
collection**. Do not hand-thread `value_roots` and re-read locals — build values
through a **handle scope** instead. Every handle minted in the scope lives in a
collector-traced arena and stays current across every allocation:

```rust
use otter_vm::{NativeCtx, NativeError, Value};

#[dive(name = "describe", length = 1)]
pub fn describe(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let port = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
    ctx.scope(|ctx, s| {
        let obj = ctx.scoped_object(s)?;                       // %Object.prototype%
        let href = ctx.scoped_string(s, "http://localhost/")?;
        ctx.scoped_set(s, obj, "href", href)?;                // allocate freely…
        let port_value = ctx.scoped_number(s, port);
        ctx.scoped_set(s, obj, "port", port_value)?;          // …handles stay current
        Ok(ctx.escape(obj))                                   // hand off to the VM
    })
}
```

The rules, the full method reference, and the GC-stress test recipe are in the
contributor guide: **[Handle Scopes: Building JS
Values](../../docs/site/src/content/docs/extensions/handle-scopes.md)**. If you
find yourself typing `value_roots`, `slice_roots`, or re-reading a local
"because GC might have moved it", stop and use `ctx.scope`.

## Generated glue

The installer glue the macros emit builds the namespace / class object through
the `bootstrap::*_with_value_roots` allocators (and, for namespaces,
`NamespaceBuilder`). Every install allocation threads the object being assembled
as a rooted `Value` by reference, so the collector keeps those handles current
and each raw-handle use re-resolves the object's live location — macro users
inherit sound value construction for free. The glue runs at bootstrap, before
user code; your method bodies run afterward and must use `ctx.scope` as above.
