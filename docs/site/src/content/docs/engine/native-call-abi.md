---
title: "Otter Native Call ABI (frozen 2026-05-23, Task 2.5)"
---

This document is the authoritative contract for every native-Rust function
that the Otter VM invokes from JS. It is the surface that the macro layer
(Phase 4) and a future JIT (Phase 2+) target. Bindings outside this
contract are not portable across those layers and will not be accepted.

## Entry shape

```rust
pub type NativeFastFn =
    for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;
```

- `&mut NativeCtx<'rt>` — handed in by the dispatcher; lifetime `'rt`
  binds the call to the active mutator turn. `NativeCtx` is `!Send +
  !Sync` and never crosses `.await`.
- `&[Value]` — call-site argument slice, borrowed for the call only.
  Native code must not retain the slice or any reference into it past
  return.
- Return: `Ok(Value)` for a normal completion, `Err(NativeError)` for
  any non-completion the dispatcher must surface.

This signature is frozen. Adding parameters, switching to async, or
changing the return shape requires a new ABI version.

## Receiver and `new.target`

`NativeCtx` carries the call-site metadata in a [`NativeCallInfo`]
record:

| Accessor | Returns | Notes |
|---|---|---|
| `ctx.this_value()` | `&Value` | The receiver bound at the call site. |
| `ctx.new_target()` | `Option<&Value>` | `Some` iff invoked via `new`. |
| `ctx.is_construct_call()` | `bool` | Sugar for `new_target().is_some()`. |
| `ctx.execution_context()` | `Option<&ExecutionContext>` | Owning module / bytecode container, when the dispatch path has one. |

These accessors return snapshots: native code may inspect them
synchronously, but must not store or move them into async work or
across calls.

## Arguments

`args: &[Value]` is the spec arguments list per §10.2.1.1 step 5
(`PrepareForOrdinaryCall`).

- Length is the number of arguments the call site actually passed. The
  `length` declared in the native's spec is a hint for the `.length`
  property, not a runtime guarantee. Missing arguments are not
  defaulted to `undefined`.
- Trailing arguments beyond the declared `length` are included
  verbatim; native code must read indices defensively.
- `Value` is `Copy` and 8 bytes; reading `args[i]` does not need any
  rooting.
- The slice is borrowed; copy out the values you need before any GC
  point.

## Return protocol

`Ok(Value)` returns a value to the JS caller. The dispatcher writes it
into the caller's destination register (or settles the result promise
for an async return) and resumes at the caller's next pc.

For constructors (`is_construct_call()` is `true`):

- Returning `Ok(Value::object(obj))` (or any object-shaped value) hands
  the caller that value.
- Returning a non-object completion is replaced by the
  `construct_target` (the freshly allocated `this`) — exactly per
  §10.2.1.4.2 step 14.

## Throw protocol

`Err(NativeError)` surfaces a non-completion to the dispatcher. The
variants and their dispatch routes are frozen:

| Variant | Dispatcher action |
|---|---|
| `NativeError::Thrown { name, message }` | Routed through the same path as `Op::Throw`: catchable by user-level `try { … } catch { … }`. |
| `NativeError::TypeError { name, reason }` | Surfaces as `VmError::TypeMismatch` (also catchable). |
| `NativeError::SyntaxError { name, reason }` | Surfaces as `VmError::SyntaxError` (catchable; used by `Function` ctor / dynamic source parsing). |
| `NativeError::RangeError { name, reason }` | Surfaces as a JS `RangeError` (catchable; required by `Number.prototype.toFixed`, `toExponential`, `toPrecision`). |
| `NativeError::Exit { code }` | Host-visible runtime termination. **Not catchable by user code.** |

`NativeError` deliberately does not carry a `Value`-shaped payload for
the `Thrown` variant; storing JS values inside `NativeError` would
cross the `!Send` boundary that `tokio::spawn` rejects. Use
`NativeError::Thrown { message }` to capture a rendered string, or
allocate the error object via the high-level helpers and `throw` it
through the dispatch context.

## Allocation

All allocation must go through the `NativeCtx` helpers, which keep the
caller's arguments and the constructed value's intermediates rooted
across the GC point:

| Surface | Use case |
|---|---|
| `ctx.alloc_object_with_roots(value_roots, slice_roots)` | Plain object with extra GC roots. |
| `ctx.alloc_host_object_with_roots(value_roots, slice_roots)` | Object backed by `HostObjectData`. |
| `ctx.alloc_map()` / `alloc_set()` / `alloc_weak_*()` | Collection bodies. |
| `ctx.alloc_iterator_state(state, roots)` | Iterator state body. |
| `ctx.alloc_weak_ref(target, roots)` | Weak reference. |
| `ctx.alloc_finalization_registry(callback, roots)` | Finalization registry. |
| `ctx.array_from_elements(elements)` / `_with_roots` | Plain dense array. |
| `ctx.array_push(arr, value, roots)` / `array_set(arr, idx, value, roots)` | Array mutation that may grow. |
| `ctx.fulfilled_promise_with_roots(value, roots)` | Settled promise. |
| `ctx.queue_microtask(callee, this, args, capability)` | Microtask enqueue. |

`ctx.heap()` and `ctx.heap_mut()` exist as an escape hatch for code
that needs the raw `GcHeap` for one of the in-progress migrations
(currently \~150 call sites). New native bindings **must** prefer the
high-level helpers above; raw `heap_mut()` access will move to
`pub(crate)` once the migration finishes. Touching the heap without
threading through the rooting helpers above is the canonical way to
introduce a use-after-free bug.

## Rooting rules

- Anything live across a GC point inside the native body must be
  rooted. The `*_with_roots` family takes `&[&Value]` plus
  `&[&[Value]]` so callers can declare both individual values and
  borrowed slices.
- The dispatch loop has already rooted the active call frame, its
  registers, and the receiver. Native code only needs to root values
  it allocates or extracts inside the body, before any further
  allocation.
- `ctx.with_gc_session(|session| { … })` opens a branded session for
  multi-step allocation sequences that share roots.

## Microtasks and promises

`ctx.queue_microtask(...)` enqueues onto the per-interpreter queue.
The queue runs FIFO within one generation per ECMA-262 HostEnqueuePromiseJob.

`ctx.fulfilled_promise_with_roots(value, ...)` is the only
sanctioned way to produce a settled promise from native code; the
returned `JsPromiseHandle` carries the cached reaction set the
dispatcher expects.

Hosting async work (timers, dynamic import, fetch) must go through
the runtime layer's host adapters — never directly through `tokio::spawn`
from native code. The `Send + 'static` bound `tokio::spawn` requires
is statically rejected by the `!Send + !Sync` boundary on `NativeCtx`,
`RuntimeCx`, `GcHeap`, `Value`, `Frame`, and every GC handle.

## Forbidden patterns

The following are compile errors (enforced by
`crates/otter-vm/tests/compile_fail/`):

- Holding `&mut NativeCtx<'_>` or `&mut RuntimeCx<'_>` across an
  `.await`.
- Capturing a raw `Gc<T>` / `Local<'gc, T>` / `Value` / `Frame` /
  `JsPromiseHandle` in a `Send + 'static` future (e.g. inside
  `tokio::spawn`).
- Cross-isolate `Gc<T>` or branded session roots.
- Constructing a `Gc<T>` outside the GC heap allocator path.
- Importing `otter_gc::raw::RawGc` from non-GC crate code.

## Versioning

This ABI is v1. Breaking changes require:

1. A new `NativeFastFn`-shaped signature with an explicit version tag
   on the registry entry, and
2. A migration plan that brings every existing native binding (Otter
   modules, intrinsics, web crate) to the new signature in a single
   cut-over.

Additive changes (new high-level allocator on `NativeCtx`, new
`NativeError` variant) are non-breaking and can land without bumping
the version.

## See also

- [`crates/otter-vm/src/runtime_cx.rs`](../crates/otter-vm/src/runtime_cx.rs)
  — `NativeCtx` / `NativeCallInfo` implementation.
- [`crates/otter-vm/src/native_function.rs`](../crates/otter-vm/src/native_function.rs)
  — `NativeFastFn`, `NativeCall`, `NativeError`, `NativeFunction`.
- [`crates/otter-vm/tests/compile_fail/`](../crates/otter-vm/tests/compile_fail/)
  — every forbidden pattern enforced at compile time.
