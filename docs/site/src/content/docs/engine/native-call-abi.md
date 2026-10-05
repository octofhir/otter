---
title: "Otter Native Call Contract"
---

This document is the authoritative contract for every native-Rust function
that the Otter VM invokes from JS. Builtins, generated surfaces and host
bindings use this same mutator-bound entry. Compiled execution reaches the
high-level native context through the engine call boundary.

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

Otter keeps one current native call shape. The dispatcher, macros, builtins,
tests, and documentation change together when the shape changes; no parallel
compatibility ABI is retained inside the active runtime.

## Compiled activation ownership

Interpreter, Template and optimized execution share one published native frame
chain. Every bytecode function generation is entered through one JavaScript
call ABI: the context, the callee, the receiver as given, `new.target`
(`undefined` exactly for `[[Call]]`), the actual count and the actual span on
the caller's stack. The span contains exactly the actual arguments; alignment
slack is never a formal argument. The callee initializes missing formals to
`undefined` in its register window.

- A compiled generation's entry builds its one native frame in the callee
  prologue, publishes the activation record inside it, binds the receiver,
  and unpublishes the record and completes `[[Construct]]` in its epilogue.
  The record names the caller's span as its actual arguments; nothing is
  copied for the callee.
- A Template frame carries the interpreter register window it executes on.
  An optimized frame keeps its values in registers and spill slots, rooted
  precisely at each safepoint. A body that runs no baseline operation on the
  window publishes its record without a window when enough actuals are present.
  An underarity entry initializes and publishes the reserved window before
  receiver conversion or constructor allocation, so missing-formal loads use
  the same unconditional register-base access. The side exit that rebuilds an
  unpublished interpreter frame publishes the reserved window, and the
  interpreter continues on the same record.
- A caller that proved its callee enters the current generation through the
  target's permanent function entry cell. The proof is one compare against
  the call site's identity cell, which the code object retains and the
  collector rewrites: it holds the last callee the full identity proof
  accepted there, and any other value takes that proof and replaces it.
- Every other callee, and every interpreter destination, enters the generic
  entry. A bytecode function with a compiled generation is entered directly,
  including underarity calls; anything else is classified in the call trampoline:
  bound and class wrappers, host frames for native, proxy and other
  callables, and interpreter frames with their windows.

Inlined functions retain source positions and moving roots in code-owned
safepoint recipes. Committed runtime operations resolve their source function
and PC from those recipes; they do not create temporary physical frames.
Exact deoptimization creates missing interpreter activations through the same
trampoline. Stack diagnostics recover inline sources from the current code
object and safepoint.

## Platform C boundaries

The x86-64 JavaScript convention is private engine plumbing on every platform.
Generated calls through a function entry cell, generic JavaScript classification,
and proper tail calls retain that convention and the actual-only stack span.
Rust runtime helpers and external function-entry/OSR transfers use the platform
C ABI. Their descriptor determines the result representation and fixed physical
argument shape; packed `Variadic` sites specify their physical count explicitly.

System V helpers return the existing `NativeResultPair` in `rax`/`rdx`.
Microsoft x64 helpers receive a hidden aggregate-result pointer, reserve shadow
space and place the shifted trailing arguments on the stack. The emitter reads
that same pair back into `rax`/`rdx` and releases its temporary call area. Scalar
word and floating-point leaves follow their scalar C signatures. The private
frame's published root pointer does not depend on the temporary C call area.
External Microsoft entries preserve `rdi`/`rsi` and all 128 bits of
`xmm6`–`xmm15` around private execution. Internal JavaScript entries bypass the
external wrapper.

Template and Graph share `SpillArea`, `ActivationExits` and `CallEntryCold`, and
one native-frame emitter per architecture. Tagged canonical homes are initialized
before publication. A tier transfer saves and restores the interpreter frame's
previous root words; an underarity call publishes its initialized window before
any collecting receiver or constructor helper.

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
- `Value` is `Copy` and 8 bytes. A copied value remains current only until
  the next collecting or reentrant operation.
- Park every argument needed after such an operation with `scope.value`,
  before the first allocation. A handle read resolves its collector-updated slot.

## Return protocol

`Ok(Value)` returns a value to the JS caller. The dispatcher writes it
into the caller's destination register (or settles the result promise
for an async return) and resumes at the caller's next pc.

For constructors (`is_construct_call()` is `true`):

- Returning `Ok(Value::object(obj))` (or any object-shaped value) hands
  the caller that value.
- Returning a non-object completion is replaced by the
  activation's receiver (`this`), as required by
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

## Allocation and rooting

Build values inside `ctx.scope(|mut scope| …)`. Its `NativeScope` builders park
every result in a collector-traced `Local`; moving collection rewrites that
handle's arena slot. Park incoming arguments with `scope.value` and use
`scope.this()` for the receiver before allocating or calling JavaScript.

```rust
ctx.scope(|mut scope| {
    let object = scope.object()?;
    let text = scope.string("ready")?;
    scope.set(object, "state", text)?;
    Ok(scope.finish(object))
})
```

Resolve handles for each operation. A raw `Value`, object offset, or copied
argument must never survive a later allocation, slab growth, getter, or setter.
The dispatcher roots the active call's values; those roots cannot update an
untracked copy in a Rust local. `scope.finish` consumes the scope to hand one
completed value directly back to the dispatcher.

Interpreter-internal drivers use `Interpreter::with_handle_scope` over the
same arena. `Array.prototype.push`, for example, roots its receiver and all
pending arguments before reading array-like length, growing a dense slab, or
performing a generic `Set`; subsequent writes reread those handles after any
collection or setter reentry. Low-level `*_with_roots` internals own their
pending operands and write barriers. New native bindings use handle scopes
instead of manual root slices.

See [Handle Scopes: Building JS Values](/extensions/handle-scopes/) for the
construction contract and stress checks.

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

## Changing the contract

Change `NativeFastFn`, every binding, the macro output, and the compile-time
tests in one cut-over. The active runtime keeps no version tag, adapter table,
or old call path.

## See also

- [`crates/otter-vm/src/runtime_cx.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-vm/src/runtime_cx.rs)
  — `NativeCtx` / `NativeCallInfo` implementation.
- [`crates/otter-vm/src/native_function.rs`](https://github.com/octofhir/otter/blob/main/crates/otter-vm/src/native_function.rs)
  — `NativeFastFn`, `NativeCall`, `NativeError`, `NativeFunction`.
- [`crates/otter-vm/tests/compile_fail/`](https://github.com/octofhir/otter/tree/main/crates/otter-vm/tests/compile_fail)
  — every forbidden pattern enforced at compile time.

Template source work is charged once per activation: the call-entry prologue
adds the body's opcode count to the function-owned saturating `SourceWork`
scalar, and loops charge at back-edge polls. Only baseline entries charge it;
the activation record carries no per-operation work state. The aligned compiled
loads/stores obey the scalar's single-mutator relaxed atomic-word contract;
code objects retain the exact source allocation throughout active retirement.
