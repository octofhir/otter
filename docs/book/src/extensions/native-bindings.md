# Native Bindings

Native bindings run on the isolate mutator thread and receive explicit
runtime context. They must not reach for thread-local heap state or move VM
/ GC handles into Rust futures.

The production direction is:

- use `NativeCtx` for allocation and mutation;
- use task-96 specs/builders to expose functions, classes, namespaces, and
  accessors;
- prefer static native function pointers for builtins;
- enforce capabilities at the Rust boundary before starting host work;
- for async host work, copy owned host data, create an operation id and
  pending promise, run the async phase without VM references, then post a
  completion back to the isolate.

Macros may eventually reduce boilerplate, but they are syntax sugar over
static specs and builders. Manual code is preferred when capability checks,
bootstrap order, or async scheduling must stay explicit.
