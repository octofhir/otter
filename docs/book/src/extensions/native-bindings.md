# Native Bindings

Native bindings run on the isolate mutator thread and receive explicit
runtime context. They must not reach for thread-local heap state or move VM
/ GC handles into Rust futures.

The current safe path is:

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
Production builtins should use
`NativeCall::Static(NativeFastFn)` through `MethodSpec` by default.
Dynamic closures are reserved for embedder cases that need captured Rust
state and can still trace explicit JS captures.

Hosted module namespace installers may use `ObjectBuilder` plus
`NativeCall::Dynamic` when the native function needs owned runtime state,
such as a cloned capability set or an `Arc<Mutex<...>>` around host-owned
database state. The closure must not capture `RuntimeCx`, `NativeCtx`,
`Value`, `Gc<T>`, `Local<'gc, T>`, frames, or handle scopes.

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
use otter_vm::{ConsoleLevel, ConsoleSink};

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
fn read_flag(
    ctx: &mut otter_vm::NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, otter_vm::NativeError> {
    check_permission(ctx, "env")?;
    let name = expect_string(args.first())?;
    let value = read_allowed_env(name)?;
    let heap = ctx.interp_mut().string_heap_clone();
    Ok(otter_vm::Value::String(otter_vm::JsString::from_str(&value, &heap)?))
}
```

This snippet is shape-only because string/value helper names continue to
move. The stable rule is that permission checks and allocation happen
through the explicit native context.

To expose that function as a static builtin, put it behind a spec and let
bootstrap or a mutator-bound builder install it:

```rust,ignore
use otter_vm::{Attr, MethodSpec, NativeCall};

static READ_FLAG: MethodSpec = MethodSpec {
    name: "readFlag",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(read_flag),
};
```

## Async Native Shape

```rust,ignore
fn start_async_read(ctx: &mut otter_vm::NativeCtx<'_>, path: PathBuf) -> Result<OpId, Error> {
    check_read_permission(ctx, &path)?;
    let op_id = create_pending_promise(ctx)?;
    let handle = ctx.interp_mut().runtime_handle().clone();

    handle.spawn_host_op(RuntimeLiveness::Ref, Box::pin(async move {
        let result = std::fs::read_to_string(path).map_err(|err| err.to_string());
        HostOpCompletion {
            id: 0,
            kind: "fs.readText".to_string(),
            result,
        }
    }));

    Ok(op_id)
}
```

The future owns `PathBuf` and strings only. It does not capture
`NativeCtx`, VM values, handles, or heap references.

Macros may eventually reduce boilerplate, but they are syntax sugar over
static specs and builders. Manual code is preferred when capability
checks, bootstrap order, or async scheduling must stay explicit.
