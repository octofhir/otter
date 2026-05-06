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

Task 96 will add specs/builders for exposing functions, classes,
namespaces, and accessors. Production builtins should use the static native
function-pointer path once that backend lands. Dynamic boxed closures are
reserved for embedder cases that need captured Rust state and can still
trace explicit JS captures.

## Synchronous Native Shape

```rust,ignore
fn read_flag(
    ctx: &mut otter_vm::NativeCtx<'_>,
    args: &[otter_vm::Value],
    _captures: &[otter_vm::Value],
) -> Result<otter_vm::Value, otter_vm::NativeError> {
    check_permission(ctx, "env")?;
    let name = expect_string(args.first())?;
    let value = read_allowed_env(name)?;
    Ok(otter_vm::Value::String(ctx.interp_mut().intern_string(&value)?))
}
```

This snippet is shape-only because string/value helper names continue to
move while Task 96 is open. The stable rule is that permission checks and
allocation happen through the explicit native context.

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
