---
title: "Handle Scopes: Building JS Values"
---

This is the standard way to build JS values from Rust in otter. If you are
adding a builtin, a Web API, or an `otter:*` module surface — start here.

## Why this API exists (read this once)

Otter's young generation is a **moving** collector (Cheney semispace): any
allocation may relocate any young object. `Value`, `JsObject`, and `JsString`
are plain `Copy` cage offsets — so a value held in a Rust local **goes stale
the moment a later allocation triggers a collection**. The failure is silent
and lands far from the cause: the stale offset survives one scavenge via its
forwarding pointer, the semispace flips, and the *next* scavenge "launders"
the dangling offset into whatever live object now occupies that memory. You
get missing properties, wrong objects, or an out-of-bounds panic three calls
later.

The historical contract — thread `value_roots: &[&Value]` into every
allocating call and re-read every local afterwards — proved impossible to
follow by hand (a single reflection path accumulated three such bugs). The
handle scope replaces it: handles live in a **collector-traced arena**, the
collector rewrites the slots in place on every collection, and a handle read
is therefore never stale. The `'s` lifetime pins every handle to its scope,
so the compiler rejects code that lets one escape.

## The API in one example

```rust
use otter_vm::{NativeCtx, NativeError, Value};

fn build_config(ctx: &mut NativeCtx<'_>, port: u16) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let obj = scope.object()?;                       // %Object.prototype%
        let href = scope.string( "http://localhost/")?;
        scope.set( obj, "href", href)?;
        let port_value = scope.number( f64::from(port));
        scope.set( obj, "port", port_value)?;

        let items = scope.array( 2)?;
        let first = scope.string( "a")?;
        scope.set_index( items, 0, first)?;
        scope.set( obj, "items", items)?;

        Ok(scope.finish(obj))                                    // hand off to the VM
    })
}
```

Every local minted through `scope` is rooted for the whole closure. Allocate in
any order, let collections fire wherever they like — every handle keeps
resolving to the current location of its object.

## Method reference

Creation methods on `NativeScope` park each result and return `Local<'scope>`:

| Method | Produces |
|---|---|
| `string(&str)` | JS string |
| `object()` / `bare_object()` | ordinary object with the default or null prototype |
| `object_with_prototype(local)` | object with a rooted explicit prototype |
| `array(len)` | array of `len` holes |
| `host_object(data)` | host-backed object |
| `number(f64)` / `boolean` / `undefined` / `null` | immediates |
| `value(value)` | parks an incoming raw `Value` before allocation |

Access resolves locals through their current arena slots:

| Method | Does |
|---|---|
| `get(obj, key)` | property read returning a new local |
| `global_binding(name)` | ordinary global identifier read, including script lexical bindings, TDZ and unresolvable-name errors |
| `global_binding_typeof(name)` | observable `typeof` global read, suppressing only unresolvable names |
| `typeof_kind(local)` | canonical nonallocating ECMAScript `typeof` classification |
| `set(obj, key, value)` | ordinary property assignment |
| `define(obj, key, value, flags)` | defines explicit data attributes |
| `set_index(array, index, value)` | array element write |
| `string_value(local)` | returns an owned Rust string |
| `object_allocation_bytes(local)` | reads the actual ordinary cell footprint |
| `finish(local)` | synchronous value handoff, valid until the next allocation |

`object_allocation_bytes` includes the GC header and inline field region. It
excludes separate slabs, exotic state and external storage; it is not a retained
heap estimate. The query performs no JS allocation or reentry and returns only
an owned scalar.

`global_binding` follows the active realm's global environment: script lexical
bindings precede the global object record; TDZ and unresolvable names throw.
`global_binding_typeof` applies the special `typeof` rule only to unresolvable
names. Object-record getters run normally. `global(name)` is
the existing nonobservable bootstrap probe; use `global_binding` when replacing
an observable identifier read. Keep repeated reads separate when getters can
replace the receiver or callable between a guard and the call.

A top-level engine host extent may consume its `NativeError` through
`NativeCtx::take_native_error` into the existing owned `RunError`, then use the
runtime's canonical diagnostic mapping. The conversion delegates the VM's one
native-error projection and transfers actual pending source frames and detail.
An already-thrown JS value retains its pending root; a bare native error class
uses the canonical intrinsic materializer. This boundary does not give nested
contributors permission to clear an enclosing call's pending error state.

Interpreter-internal code (inside `otter-vm`) uses the same core via
`Interpreter::with_handle_scope` + the `scoped_*` methods on `Interpreter`.

## The rules

1. **Never hold a raw `Value`/`JsObject`/`JsString` across an allocating
   call.** If you receive one (an argument, a property read), park it with
   `scope.value` before your first allocation and use the handle from then
   on.
2. **`finish` is a hand-off, not a loophole.** The raw `Value` it returns is
   valid only until the next allocation. Return it to the VM immediately or
   store it into an already-rooted object. Never stash it in a local and keep
   allocating.
3. **Don't re-derive raw values inside the scope.** Resolve through the
   handle every time (`scope.get`, `scope.string_value`, …). The whole point is
   that the arena slot is the single source of truth.
4. **Flags must match the surface you're building.** `scope.set` performs
   ordinary assignment; spec'd attributes go through
   `scope.define` with explicit `Attr::…().to_flags()` — copy the
   attribute choices from the code you're replacing or the spec text.
5. **Scopes nest freely.** Open an inner scope for per-iteration temporaries
   in a loop; its handles die at the inner boundary, the outer ones survive.
   Keeps the arena small in long loops.

## What NOT to write

```rust
// BROKEN: every later alloc can move the earlier strings.
let href = string_value(ctx, &href_text)?;
let proto = string_value(ctx, &proto_text)?;    // href may be stale now
let obj = ObjectBuilder::from_host_data(ctx, state)?;  // both may be stale now
```

```rust
// SOUND BUT WRONG STYLE: manual value_roots threading + re-reads.
// Don't write this; use ctx.scope.
let desc = self.some_helper_runtime_rooted(ctx, v, &[&a, &b], &[slice])?;
a = a_root.as_object().unwrap(); // manual re-read after the rooted call
```

If you find yourself typing `value_roots`, `slice_roots`, or re-reading a
local "because GC might have moved it" — stop and use a scope.

## Testing your native

Every native that builds multiple values must hold up under GC stress:

```bash
cargo build --release -p otter-cli
for s in 1 2 4 8 16; do
  OTTER_GC_STRESS=$s target/release/otter -e '<exercise your surface>'
done
# outputs must be identical for every stride, and identical to stride-0
OTTER_GC_VERIFY=1 OTTER_GC_STRESS=8 target/release/otter -e '...'   # no "corrupt slot" lines
```

`OTTER_GC_STRESS=N` forces a scavenge every N allocations, so any unrooted
handle fails deterministically instead of once a week in production.

## Escape-proofing is compiler-enforced

`Local<'scope>` borrows the scope token; letting a handle outlive its scope is
a compile error:

```rust
let leaked = ctx.scope(|ctx, s| scope.string( "x").unwrap());
// error[E0597]: borrowed value does not live long enough
```

## Codebase invariant

No raw GC handle is held across an allocation, except inside `*_with_roots`
internals that root explicitly. `ObjectBuilder` now reaches its object only
through a private rooted slot that every allocating method threads into the
collection root set, so no builder method can dereference a stale handle even if
the allocations are reordered; `NamespaceBuilder` delegates to it. The binding
macros (`couch!`, `holt!`, …) emit static specs plus install glue that builds
through the by-reference `bootstrap::*_with_value_roots` allocators — the object
being assembled is rooted across every install allocation, so macro users
inherit sound value construction and never touch raw handles. Your method bodies
build values with `ctx.scope` (above).
