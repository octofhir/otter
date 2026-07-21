---
title: "Native Bindings"
---

Native bindings run on the isolate mutator thread and receive explicit
runtime context. They must not reach for thread-local heap state or move VM
/ GC handles into Rust futures.

The current safe path is:

- build multi-allocation JS values inside a handle scope — `ctx.scope` +
  `scoped_*` — so no raw `Value` local is held across an allocation (see
  [Handle Scopes: Building JS Values](/extensions/handle-scopes/));
- use `NativeCtx` for allocation and mutation;
- use `NativeCtx::record_write` or higher-level container helpers after
  storing GC-bearing values;
- use `NativeCtx::reserve_external` for host buffers and backing stores;
- use `NativeCtx::with_gc_session` for branded roots and weak handles;
- enforce capabilities at the Rust boundary before starting host work;
- for async host work, copy owned host data, create an operation id and
  pending promise, run the async phase without VM references, then post a
  completion back to the isolate.

Specs/builders expose functions, classes, namespaces, and accessors.
Production builtins should use runtime-owned helpers such as
`runtime_method(...)`, `RuntimeNativeScope::native_method(...)`, or
`runtime_native_static(...)` by default. Dynamic closures are reserved for
embedder cases that need captured Rust state and can still trace explicit JS
captures.

Hosted module namespace installers receive the caller's
`RuntimeNativeScope`, capability set, and optional task spawner. Build and
return the namespace as a `RuntimeLocal`; create receiver-backed resources with
`scope.host_object(data)`. Namespace-level closures may capture owned
configuration such as a cloned capability set, but per-instance state should
live on the JS object and be reached through `scope.with_host_data(_mut)` or,
in a low-level callback, `runtime_this_object(...)` plus
`runtime_with_host_data(_mut)`. Closures must not capture
`RuntimeCx`, `NativeCtx`, `Value`, `Gc<T>`, `Local<'gc, T>`, frames, or handle
scopes.

If host state contains JavaScript references, implement
`RuntimeTracedHostObjectData` and allocate it with
`RuntimeNativeScope::traced_host_object`. Untraced payload types explicitly
implement `RuntimeHostObjectData`; this opt-in asserts that they contain no JS
references. Store each traced reference in a
`RuntimeHostValueSlot`; its only mutation path is
`set_host_data_value`, which performs the normal generational write barrier.
The tracer exposes only these opaque slots, so host code cannot trace arbitrary
memory or manufacture raw GC pointers. Both payload kinds are finalized when
their object dies or the runtime is disposed.

Repeated property access should intern the name once with
`HostAtomInterner::intern`, then use `RuntimeNativeScope::{get_atom,set_atom}`.
`RuntimeHostAtom` is clone-cheap, stable, and `Send + Sync`; it stores no
isolate pointer. For temporary string inspection use
`RuntimeNativeScope::with_string_str`: Latin-1 ASCII is borrowed for the
callback and other encodings use an owned fallback, while the callback lifetime
prevents a heap borrow from escaping.

Source/module loading is separate from filesystem I/O permissions. Following
Deno's model, the entrypoint and statically analyzable local module graph are
code loading, not `fs_read`. Runtime APIs that expose arbitrary file reads
must still enforce `CapabilitySet::read`. Dynamic local imports follow the
same module-loader policy as static imports. Remote static and dynamic imports
share the async provider pipeline and enforce the runtime's network capability
before transport begins; embedders may add origin and CORS policy in their
provider.

## Embedder Console Sink

`globalThis.console` is installed through the same static namespace spec
path as other builtins, but its output target is embedder-overridable. The
default runtime config uses `StdConsoleSink`, which writes `log`, `info`,
and `debug` with `println!`, and `warn`, `error`, `trace`, and failed
`assert` with `eprintln!`.

Embedders that need structured logging can provide a sink while building
the runtime:

```rust,ignore
use std::sync::Arc;
use otter_runtime::{ConsoleLevel, ConsoleSink};

#[derive(Debug)]
struct TracingConsole;

impl ConsoleSink for TracingConsole {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        tracing::info!(?level, message = fields.join(" "));
    }
}

let otter = otter_runtime::Otter::builder()
    .console_sink(Arc::new(TracingConsole))
    .build()?;
```

The sink receives already-rendered JS argument fields in call order. It
must not store VM values, GC handles, or native contexts.

## Throwing an existing JavaScript value

DOM and Web-IDL bindings sometimes need to throw an object they already
constructed, such as a `DOMException`. Use `NativeCtx::throw_value` and return
the error immediately:

```rust,ignore
fn remove_child(ctx: &mut NativeCtx<'_>, exception: Value) -> Result<Value, NativeError> {
    Err(ctx.throw_value("Document.removeChild", exception))
}
```

Constructing `NativeError::Thrown` directly preserves only display text.
`throw_value` stages the live value in the isolate's traced throw slot, so a
JavaScript `catch` receives the same object, symbol, or primitive value.

## Synchronous Native Shape

```rust,ignore
use otter_runtime::{
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeValue as Value,
    runtime_arg_to_string, runtime_string_value,
};

fn read_flag(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    check_permission(ctx, "env")?;
    let name = runtime_arg_to_string(args, 0);
    let value = read_allowed_env(name)?;
    runtime_string_value(ctx, &value)
}
```

This snippet is shape-only because string/value helper names continue to
move. The stable rule is that permission checks and allocation happen
through the explicit native context.

To expose that function as a static builtin, put it behind a spec and let
bootstrap or a mutator-bound builder install it:

```rust,ignore
use otter_runtime::{
    RuntimeMethodSpec as MethodSpec, runtime_method,
};

static READ_FLAG: MethodSpec = runtime_method("readFlag", 1, read_flag);
```

## Async Native Shape

```rust,ignore
use otter_runtime::RuntimeNativeCtx as NativeCtx;

fn start_async_read(ctx: &mut NativeCtx<'_>, path: PathBuf) -> Result<OpId, Error> {
    check_read_permission(ctx, &path)?;
    let op_id = create_pending_promise(ctx)?;
    queue_owned_host_request("fs.readText", op_id, path);

    Ok(op_id)
}
```

The host request owns `PathBuf`, ids, and strings only. It does not capture
`NativeCtx`, VM values, handles, or heap references. Completion must return
through a typed runtime inbox message or service result, and promise
settlement happens back on the isolate thread.

Layer B can share an embedder-owned Tokio executor instead of creating a
second one:

```rust,ignore
let runtime = Runtime::builder()
    .tokio_handle(application_runtime.handle().clone())
    .build_handle()?;
```

Layer A remains driven by the embedder's loop. Install a
`HostCompletionSink` that spawns the owned future and posts each
`HostCompletionJob` into that loop. When the event reaches the runtime's owning
thread, call `Runtime::run_host_completion(job)`. Never run the completion job
on a Tokio worker; it re-enters the isolate and may touch the GC heap.

Macros may eventually reduce boilerplate, but they are syntax sugar over
static specs and builders. Manual code is preferred when capability
checks, bootstrap order, or async scheduling must stay explicit.
