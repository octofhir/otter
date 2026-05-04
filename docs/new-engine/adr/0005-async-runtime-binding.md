# ADR-0005 — Async Runtime Binding and Event Loop

- **Status:** accepted
- **Date:** 2026-05-04
- **Deciders:** project lead
- **Related:**
  - [`0003-public-api-and-cli.md`](./0003-public-api-and-cli.md)
  - [`0004-gc-crate-and-unsafe-boundary.md`](./0004-gc-crate-and-unsafe-boundary.md)
  - [`../gc-architecture.md`](../gc-architecture.md)
  - [task 76A](../tasks/76a-runtime-binding-explicit-context.md)
  - [task 85](../tasks/85-tokio-event-loop-runtime-handle.md)

## Context

Otter must be convenient as a standalone JavaScript/TypeScript runtime and
as an embedded runtime, while preserving the GC invariant that one isolate
has exactly one mutator. The default product path is Tokio-based: the CLI,
tests, timers, async host operations, and future server surfaces all run on
Tokio unless an embedder explicitly supplies another event-loop backend.

The current new-engine GC has a thread-local heap lookup helper
(`GcHeap::enter_thread_default` / `with_thread_default`). That is useful as
a migration sketch, but it is not a stable architecture: Tokio futures may
move between worker threads, a thread-local raw heap pointer cannot prove
which isolate owns a handle, and a future must never retain VM or GC
borrows across `.await`.

## Decision

### 1. Public API is handle-first

The public `Otter` type is a cloneable, async-friendly facade. It is
`Send + Sync` and internally holds a `RuntimeHandle`.

```
Otter              // public facade; Clone + Send + Sync
  -> RuntimeHandle // command / completion API
    -> IsolateRunner
      -> RuntimeCore / Interpreter / RuntimeState / GcHeap // !Send + !Sync
```

`Otter::new()` uses the default Tokio event loop. `Otter::builder()` may
override event-loop settings, but the out-of-box path must work in:

- the CLI;
- a Tokio multi-thread runtime;
- tests that call `tokio::spawn`;
- a plain sync process via `blocking_run_*` helpers backed by an owned
  Tokio runtime created by the default event-loop implementation.

### 2. Tokio is the default event loop

`otter-runtime` owns an `EventLoop` trait and ships a required
`TokioEventLoop` implementation. The trait exists to keep VM internals free
from Tokio-specific types and to let embedders control scheduling,
telemetry, cancellation, and sandbox policy. It is not an excuse to defer
the default implementation.

Minimum shape:

```rust
pub trait EventLoop: Send + Sync + 'static {
    fn spawn_host_op(&self, op: HostFuture);
    fn schedule_timer(&self, request: TimerRequest) -> TimerToken;
    fn cancel_timer(&self, token: TimerToken);
    fn now(&self) -> Instant;
    fn wake_runtime(&self, wake: RuntimeWake);
}

pub struct TokioEventLoop { /* tokio::runtime::Handle or owned runtime */ }
```

### 3. VM and GC stay explicit-context and single-mutator

`RuntimeCore`, `Interpreter`, `RuntimeState`, `GcHeap`, `NativeCtx<'_>`,
`Gc<T>`, `Local<'gc, T>`, and internal `Value` handles are `!Send + !Sync`.
They never appear in a public `Send` future.

Internal VM and native APIs pass context explicitly:

```rust
obj.get(&mut cx, key)
obj.set(&mut cx, key, value)
cx.heap.write_barrier(owner, value)
```

Product code must not depend on `GcHeap` thread-local lookup. If temporary
test helpers remain, they are crate-private or `#[doc(hidden)]` and are
forbidden in runtime / native-binding code.

### 4. Async host functions split at the runtime boundary

Native bindings are not `async fn(&mut NativeCtx, ...)`. They run a
synchronous isolate phase:

1. validate arguments;
2. copy or serialize owned host data;
3. create a pending promise / host-op id;
4. register any required isolate-owned roots.

The async phase runs on the `EventLoop` without VM references. Completion
posts an owned message back to the isolate inbox. The isolate resolves or
rejects the promise on a later mutator turn.

No `&mut RuntimeState`, `&mut GcHeap`, `NativeCtx<'_>`, `Local<'gc, T>`,
`Gc<T>`, internal `Value`, or `Frame` may cross `.await`.

### 5. Workers are isolate-per-worker

A worker owns its own `RuntimeCore` and `GcHeap`. No GC handle or internal
value crosses worker boundaries. Communication uses structured clone,
transferables, message ports, and later explicit shared-memory surfaces.

Multi-core throughput comes from isolate pools / workers, not from a shared
heap or parallel JS execution inside one isolate.

### 6. Stability and early detection are required features

The runtime must include checks that fail early:

- compile-fail tests proving internal VM/GC handles cannot be captured by
  `tokio::spawn`;
- runtime isolate-id / generation assertions in debug builds;
- leak reports on runtime drop: open handle scopes, global handles, pending
  host ops, live timers, queued commands;
- GC stress modes: collect on every allocation, every safepoint, and every
  async completion;
- bounded command queues with backpressure;
- cancellation that aborts waiters / host ops without dropping the isolate
  mid-mutator-turn.

## Consequences

- ADR-0003's original "sync-only public API" decision is superseded for the
  public facade. The low-level core remains local and synchronous, but the
  product API is async-first.
- GC migration tasks must use explicit context APIs, even when method-style
  APIs would be shorter.
- The default CLI and future embedded surfaces use the same event-loop
  implementation, so async behavior is tested through the product path.
- Embedders that want custom runtimes implement `EventLoop`; they still
  receive `Otter` / `RuntimeHandle`, not VM internals.

## Alternatives Considered

- **Thread-local heap lookup as the public model.** Rejected. It cannot
  prove isolate ownership across Tokio worker migration and hides borrow
  boundaries from the type system.
- **Expose `Runtime` directly and require `LocalSet`.** Rejected for the
  public product surface. It is sound but too easy to misuse in embedded
  async applications. `LocalSet` remains an internal implementation option
  for `IsolateRunner`.
- **Make the whole runtime `Send`.** Rejected. A `Send` heap would weaken
  the single-mutator invariant and make GC/write-barrier bugs harder to
  detect.
- **Defer `EventLoop`.** Rejected. Timers, async host ops, CLI behavior,
  cancellation, and worker messaging all need one scheduling boundary from
  the start.
