# Task 85 — Tokio-first `EventLoop`, `Otter`, and `RuntimeHandle`

## Status

- [ ] `EventLoop` trait added to `otter-runtime`
- [ ] `TokioEventLoop` default implementation added and documented
- [ ] public `Otter` facade is `Clone + Send + Sync`
- [ ] `RuntimeHandle` command/completion API implemented
- [ ] isolate runner owns `RuntimeCore` / VM / `GcHeap` on one mutator
- [ ] async `run_*` APIs work from Tokio multi-thread executor
- [ ] cancellation, timeout, backpressure, and leak diagnostics covered
- [ ] gates green

## Goal

Give users the convenient product API:

```rust
let otter = Otter::new();
let result = otter.run_script("console.log('hi')").await?;
```

The call must be safe when the awaiting future is polled on different
Tokio worker threads. Only the public handle is `Send + Sync`; the isolate
and GC remain single-mutator and `!Send + !Sync`.

## Source

- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)
- [`../adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md)

## Scope

### 85.1 — `EventLoop` trait

Add the scheduling boundary to `otter-runtime`:

```rust
pub trait EventLoop: Send + Sync + 'static {
    fn spawn_host_op(&self, op: HostFuture);
    fn schedule_timer(&self, request: TimerRequest) -> TimerToken;
    fn cancel_timer(&self, token: TimerToken);
    fn now(&self) -> Instant;
    fn wake_runtime(&self, wake: RuntimeWake);
}
```

Names may change, but the trait must cover host ops, timers, wakeups,
cancellation, and time source. VM crates must not import Tokio types.

### 85.2 — `TokioEventLoop`

Implement and expose the default:

- `TokioEventLoop::current()` uses `tokio::runtime::Handle::current()`.
- `TokioEventLoop::from_handle(handle)` reuses an embedder-provided Tokio
  runtime.
- `TokioEventLoop::owned()` creates an owned runtime for plain sync
  processes and tests that call `Otter::new()` outside Tokio.
- `Otter::new()` uses `TokioEventLoop::current_or_owned()`.

### 85.3 — Public handle surface

`Otter` is a small facade over `RuntimeHandle`:

```rust
#[derive(Clone)]
pub struct Otter {
    handle: RuntimeHandle,
}
```

Required API shape:

- `Otter::new() -> Otter`
- `Otter::builder() -> OtterBuilder`
- `async fn run_script(...) -> Result<ExecutionResult, OtterError>`
- `async fn run_module(...) -> Result<ExecutionResult, OtterError>`
- `fn interrupt(&self)`
- optional `blocking_run_*` wrappers for non-async callers, implemented
  on top of the same handle.

`ExecutionResult` returned through `RuntimeHandle` must not expose internal
`Value` / `Gc<T>` handles that can outlive the isolate.

### 85.4 — Isolate runner

`RuntimeHandle` sends commands to an `IsolateRunner`. The runner owns the
local `RuntimeCore`, `Interpreter`, `RuntimeState`, and `GcHeap`.

Rules:

- one command runs to a safepoint / completion on the mutator;
- no concurrent `&mut RuntimeState`;
- bounded command queue with backpressure;
- command cancellation drops waiters or marks an op cancelled, but never
  drops the isolate mid-mutator-turn;
- host-op completions re-enter through an owned message, not direct VM
  references.

### 85.5 — Async host op bridge

Add the host-op shape described by ADR-0005:

1. synchronous isolate phase validates args and creates pending promise /
   op id;
2. async phase runs on `EventLoop` without VM refs;
3. completion posts back to isolate inbox;
4. isolate resolves/rejects promise on a later turn.

### 85.6 — Diagnostics and early detection

Add leak diagnostics when a runtime shuts down:

- live global handles;
- open handle scopes;
- pending host ops;
- live timers;
- queued commands;
- unresolved promises created by host ops.

Add stress modes:

- collect every allocation;
- collect every safepoint;
- collect after every async completion.

## Out of scope

- Worker API and structured clone (task 92).
- Phase 2 incremental marking (task 86).
- Public web server framework integration. The handle shape must be ready
  for it, but no server is built here.

## Validation gates

- [ ] `Otter` and `RuntimeHandle` satisfy `Send + Sync`; `RuntimeCore`,
  `RuntimeState`, `Interpreter`, `GcHeap`, `Gc<T>`, `Local<'gc, T>` do not.
- [ ] Tokio multi-thread test: clone `Otter`, call `tokio::spawn` from
  multiple tasks, verify runs serialize correctly and all results return.
- [ ] Cancellation test: drop the waiting future while the isolate is
  running; isolate reaches a consistent safepoint and later commands work.
- [ ] Timeout test: timeout rejects/returns `OtterError::Timeout`, no
  leaked pending host op / timer.
- [ ] Backpressure test: bounded queue refuses or awaits when full.
- [ ] Stress-GC tests green in all three stress modes.
- [ ] `cargo test -p otter-runtime -p otter-vm -p otter-gc` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.

## Closing

Tick task 85 in [70-gc-master-tracker.md](./70-gc-master-tracker.md) and
update ADR-0003 if any public method names differ from this task.
