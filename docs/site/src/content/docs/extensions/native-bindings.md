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
`runtime_method(...)`, `RuntimeObjectBuilder::builtin_method(...)`, or
`runtime_native_static(...)` by default. Dynamic closures are reserved for
embedder cases that need captured Rust state and can still trace explicit JS
captures.

Hosted module namespace installers should use `HostedModuleCtx` and attach
long-lived Rust state to receiver objects through the runtime host-object
primitive. Namespace-level closures may capture owned configuration such as a
cloned capability set, but per-instance state should live on the JS object and
be reached through `runtime_this_object(...)` plus
`runtime_with_host_data(_mut)`. Closures must not capture
`RuntimeCx`, `NativeCtx`, `Value`, `Gc<T>`, `Local<'gc, T>`, frames, or handle
scopes.

Source/module loading is separate from filesystem I/O permissions. Following
Deno's model, the entrypoint and statically analyzable local module graph are
code loading, not `fs_read`. Runtime APIs that expose arbitrary file reads
must still enforce `CapabilitySet::read`; future non-analyzable dynamic local
imports and remote imports should use an explicit import policy rather than
piggybacking on ordinary file I/O.

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

Macros may eventually reduce boilerplate, but they are syntax sugar over
static specs and builders. Manual code is preferred when capability
checks, bootstrap order, or async scheduling must stay explicit.
