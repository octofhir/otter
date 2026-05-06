# GC API

Otter's active GC is moving, generational, and isolate-local. Normal
engine and extension work should use the safe context API rather than
raw collector internals.

## Handle Tiers

- `Local<'gc, T>` is a stack-scoped root created by a handle scope.
- `EscapableHandleScope<'gc>` is the explicit way to return one
  `Local<'gc, T>` from a nested scope.
- `Root<'iso, T>` is a persistent isolate-owned root.
- `Weak<'iso, T>` is a weak handle. It can only be upgraded through a
  matching `GcSession<'iso, '_>`.
- Raw `Gc<T>` handles are VM values, not persistence handles. Do not
  store them across async, worker, or host-operation boundaries; use a
  `Root` and re-enter the owning isolate.

## Native Context

Native functions receive `NativeCtx<'_>`. The public mutable raw heap
borrow is intentionally not available to native authors. Use these
helpers instead:

```rust,ignore
fn native(
    ctx: &mut otter_vm::NativeCtx<'_>,
    _args: &[otter_vm::Value],
    _captures: &[otter_vm::Value],
) -> Result<otter_vm::Value, otter_vm::NativeError> {
    let object = ctx.alloc_old(MyBody::default())?;
    ctx.record_write(object, &otter_vm::Value::Undefined);

    let backing = ctx.reserve_external(4096)?;
    drop(backing);

    Ok(otter_vm::Value::Undefined)
}
```

Use `NativeCtx::with_gc_session` when a native path needs branded root
or weak operations:

```rust,ignore
ctx.with_gc_session(|mut session| {
    let local = session.alloc(MyBody::default())?;
    let root = session.root(local);
    let weak = session.weak(root.get(&session));
    assert!(weak.upgrade(&session).is_some());
    Ok::<_, otter_gc::OutOfMemory>(())
})?;
```

## Mutation

Do not call write barriers directly. Store the value first, then record
the store through `GcHeap::record_write` or `NativeCtx::record_write`.
The stored value implements `GcStore`, and the heap records every
outgoing GC edge without exposing raw slot pointers:

```rust,ignore
let stored = value.clone();
heap.with_payload(parent, |body| {
    body.field = value;
});
heap.record_write(parent, &stored);
```

This is the reference pattern used by object properties, array elements,
Map/Set entries, promises, generators, upvalues, and finalization
registries.

## Escaping Locals

Use `EscapableHandleScope` when a helper opens a nested handle scope and
needs to return one rooted value to the caller's scope:

```rust,ignore
let escaped = {
    let mut inner = otter_gc::EscapableHandleScope::new(heap.handle_stack());
    let local = inner.local(gc_value);
    inner.escape(&local)
};
```

## External Memory

Memory outside GC cells must be accounted with an RAII reservation:

```rust,ignore
let mut backing = heap.reserve_external(16 * 1024)?;
backing.resize(32 * 1024)?;
drop(backing); // releases the reservation
```

This covers typed-array backing stores, host buffers, large module
source caches, and native resources.

## Internal Only

The following are collector or audited VM-adapter internals:

- `RawGc`
- `TraceTable`
- raw slot visitors (`*mut RawGc`)
- `GcHeap::write_barrier_raw`
- direct handle-table mutation
- context-free weak upgrades

Raw collector types are not re-exported from the root `otter_gc` API.
Audited VM adapters may import `otter_gc::raw::*`; contributor code
should treat that module as unavailable. Compile-fail gates reject
root-level raw imports and direct raw barrier calls.
