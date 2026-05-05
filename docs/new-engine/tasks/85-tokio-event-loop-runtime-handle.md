# Task 85 — Production Tokio `EventLoop`, `Otter`, and `RuntimeHandle`

## Status

- [ ] open after GC Phase 1 safety gates needed by async roots are green
      (tasks 76A, 77-84)
- [ ] `EventLoop` trait added to `otter-runtime`
- [ ] `TokioEventLoop` default implementation added and documented
- [ ] public `Otter` facade is `Clone + Send + Sync`
- [ ] `RuntimeHandle` command/completion API implemented
- [ ] isolate runner owns `RuntimeCore` / VM / `GcHeap` on one mutator
- [ ] runtime inbox separates commands, host completions, timers, interrupts,
      and diagnostics
- [ ] event-loop drive APIs cover one tick, run-to-idle, and run-until-promise
- [ ] async `run_*` APIs work from Tokio multi-thread executor
- [ ] timers, host ops, dynamic module work, and promise settlement obey
      deterministic turn/checkpoint semantics
- [ ] cancellation, timeout, backpressure, ref/unref, and leak diagnostics
      covered
- [ ] activity stats / op metrics available for tests and debug tooling
- [ ] gates green

## Goal

Give users the convenient product API:

```rust
let otter = Otter::new();
let result = otter.run_script("console.log('hi')").await?;
```

The call must be safe when the awaiting future is polled on different
Tokio worker threads. Only the public handle is `Send + Sync`; the isolate,
VM, runtime state, and GC remain single-mutator and `!Send + !Sync`.

This is production infrastructure, not embedder polish. The runtime must be
able to drive scripts, modules, timers, async host operations, dynamic
imports, and future server APIs without VM/GC handles crossing `.await` or
worker boundaries.

Breaking Rust API changes inside `crates-next/*` are allowed when they
simplify the runtime boundary, remove unsoundness risk, reduce startup or
hot-path overhead, or make lifecycle behavior deterministic.

## Source

- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)
- [`../adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md)
- [`82-migrate-promise-iterator-generator.md`](./82-migrate-promise-iterator-generator.md)
  parked async frames and promise roots.
- Deno / `deno_core` reference insights, used as design input only:
  `JsRuntime` is `!Send` / `!Sync`, exposes `poll_event_loop`,
  `run_event_loop`, `with_event_loop_promise`, and operation/activity
  stats; Otter must preserve the same single-runtime ownership idea while
  using its own VM/GC and explicit-context rules.

## Deno-derived implementation lessons

Do not copy Deno's V8/op internals directly. Extract these lessons:

1. **Separate runtime polling from awaiting a JS promise.** A JS promise
   future cannot make progress unless the event loop is polled. Otter needs
   first-class `run_until_promise` / `run_until_idle` helpers so callers do
   not accidentally await a promise while starving host completions.
2. **One tick is a public internal primitive.** Deno has a single-tick
   `poll_event_loop` and a run-to-completion `run_event_loop`. Otter's
   `IsolateRunner` should expose equivalent internal drive modes so tests,
   debuggers, and embeddings can step deterministically.
3. **Pending work must be counted by kind.** Deno tracks pending ops,
   dynamic imports, inspector sessions, resources, and op metrics. Otter
   needs activity stats for commands, host ops, timers, microtasks, dynamic
   module work, and inspector/debug sessions.
4. **Ref/unref semantics are part of runtime liveness.** Timers and host
   ops may be referenced or unreferenced. Referenced work keeps the event
   loop alive; unreferenced work may complete if the loop is already being
   driven but must not prevent idle shutdown.
5. **Sanitizer-style leak reports are product features.** Tests and
   embedders need structured diagnostics that say which op/timer/resource
   kept the runtime alive or leaked at shutdown.
6. **Fast paths and metrics must be separable.** Always-on metrics should
   not tax hot native dispatch. Provide cheap counters where needed and
   opt-in detailed tracing/debug metadata for diagnostics.

## Scope

### 85.1 — `EventLoop` trait

Add the scheduling boundary to `otter-runtime`:

```rust
pub trait EventLoop: Send + Sync + 'static {
    fn spawn_host_op(&self, op: HostFuture) -> HostJoinHandle;
    fn schedule_timer(&self, request: TimerRequest) -> TimerToken;
    fn cancel_timer(&self, token: TimerToken) -> bool;
    fn now(&self) -> Instant;
    fn wake_runtime(&self, wake: RuntimeWake);
}
```

Names may change, but the trait must cover:

- spawning host futures;
- timer scheduling and cancellation;
- waking the isolate runner;
- time source injection for deterministic tests;
- host-op cancellation / abort handles;
- ref/unref liveness metadata;
- optional telemetry hooks.

VM crates must not import Tokio types. Tokio stays in `otter-runtime` or
product crates.

### 85.2 — `TokioEventLoop`

Implement and expose the default:

- `TokioEventLoop::current()` uses `tokio::runtime::Handle::current()`;
- `TokioEventLoop::from_handle(handle)` reuses an embedder-provided Tokio
  runtime;
- `TokioEventLoop::owned()` creates an owned runtime for plain sync
  processes and tests that call `Otter::new()` outside Tokio;
- `TokioEventLoop::current_or_owned()` is the default path;
- timers use Tokio time but take the `EventLoop` time source so tests can
  fake time later;
- spawned host futures never hold VM/GC borrows or handles.

### 85.3 — Public handle surface

`Otter` is a small facade over `RuntimeHandle`:

```rust
#[derive(Clone)]
pub struct Otter {
    handle: RuntimeHandle,
}
```

Required API shape:

- `Otter::new() -> Otter`;
- `Otter::builder() -> OtterBuilder`;
- `async fn run_script(...) -> Result<ExecutionResult, OtterError>`;
- `async fn run_module(...) -> Result<ExecutionResult, OtterError>`;
- `async fn eval(...) -> Result<ExecutionResult, OtterError>`;
- `fn interrupt(&self)`;
- `fn activity_stats(&self) -> RuntimeActivityStats` or async equivalent;
- optional `blocking_run_*` wrappers for non-async callers, implemented on
  top of the same handle.

`ExecutionResult` returned through `RuntimeHandle` must not expose internal
`Value` / `Gc<T>` handles that can outlive the isolate.

### 85.4 — Runtime command and inbox model

`RuntimeHandle` sends commands to an `IsolateRunner`. The runner owns the
local `RuntimeCore`, `Interpreter`, `RuntimeState`, and `GcHeap`.

Use an explicit message enum rather than overloading the microtask queue:

```rust
enum RuntimeMessage {
    Command(RuntimeCommand),
    HostOpCompleted(HostOpCompletion),
    TimerFired(TimerToken),
    DynamicModuleReady(ModuleJobId),
    Interrupt(InterruptReason),
    InspectorEvent(InspectorEvent),
    Shutdown,
}
```

Exact names may change. The rule is that VM microtasks remain a JS
checkpoint queue; runtime messages are host/event-loop work that may cause
a new JS turn or promise settlement.

Rules:

- one command runs to a safepoint / completion on the mutator;
- no concurrent `&mut RuntimeState`;
- bounded command queue with backpressure;
- completions re-enter through owned messages, not direct VM references;
- every message has an id / origin useful for diagnostics;
- cancellation drops waiters or marks an op cancelled, but never drops the
  isolate mid-mutator-turn.

### 85.5 — Event-loop drive modes

Implement explicit drive modes on the runner:

- `poll_one_tick`: process at most one event-loop turn / runtime message,
  then perform the required microtask checkpoint;
- `run_until_idle`: drive referenced work until no referenced commands,
  host ops, timers, module jobs, inspector sessions, or microtasks remain;
- `run_until_promise`: drive the event loop until a target promise settles
  or the loop resolves with that promise still pending;
- `run_until_command`: drive until a specific command's completion is sent;
- `shutdown`: cancel or drain according to configured policy, then report
  leaks.

Turn semantics:

1. run one JS command/callback/timer/module turn on the mutator;
2. perform a microtask checkpoint;
3. fold host completions into the runtime inbox;
4. repeat according to the selected drive mode.

Promise reaction jobs, `queueMicrotask`, async-function resume, and
`await` resume stay in the VM microtask queue. Timers, host-op
completions, dynamic import completions, and future inspector/debug events
enter through the runtime inbox.

### 85.6 — Async host op bridge

Add the host-op shape described by ADR-0005:

1. synchronous isolate phase validates args and capabilities;
2. copy or serialize owned host data only;
3. create a pending promise / host-op id and register isolate-owned roots;
4. async phase runs on `EventLoop` without VM refs;
5. completion posts an owned `HostOpCompletion` back to the isolate inbox;
6. isolate resolves/rejects promise on a later turn;
7. microtasks drain after that turn.

No `&mut RuntimeState`, `&mut GcHeap`, `NativeCtx<'_>`, `RuntimeCx<'_>`,
`Gc<T>`, `Local<'gc, T>`, internal `Value`, `Frame`, or handle scope may
cross `.await`.

### 85.7 — Timers and ref/unref liveness

Timers are runtime primitives, not Node-specific APIs. Implement the
backend needed for:

- `setTimeout`;
- `setInterval`;
- `setImmediate` if/when exposed;
- `queueMicrotask` remains VM microtask queue, not timer queue;
- future `node:timers` re-exports runtime timers rather than adding a
  separate backend.

Every timer/host op has liveness metadata:

```rust
enum RuntimeLiveness {
    Ref,
    Unref,
}
```

Referenced work keeps `run_until_idle` alive. Unreferenced work may finish
if the runtime is already being polled, but it does not prevent idle
completion or shutdown. Leak diagnostics must distinguish ref and unref
work.

### 85.8 — Dynamic modules and top-level await

Module evaluation, dynamic import, and top-level await must use the same
runtime drive model:

- module graph loading may perform host async work through `EventLoop`;
- module evaluation promises are tracked as target promises;
- `run_module` drives until the module evaluation promise settles or the
  event loop becomes idle with that promise pending;
- module namespace access after evaluation must fail cleanly if evaluation
  has not completed.

### 85.9 — Diagnostics, stats, and early detection

Add `RuntimeActivityStats` for tests and debug tooling. It should include
at least:

- queued commands;
- running command id / kind;
- pending referenced host ops;
- pending unreferenced host ops;
- pending timers by ref/unref;
- pending dynamic module jobs;
- pending microtasks / generation counter;
- unresolved host-created promises;
- cancellation count;
- timeout count;
- completed / failed host ops;
- optional per-op dispatch/completion counters.

Add leak diagnostics when a runtime shuts down:

- live global handles / roots;
- open handle scopes;
- pending host ops with op id, kind, age, liveness, and origin;
- live timers with token, deadline, repeat flag, liveness, and origin;
- queued commands;
- unresolved promises created by host ops;
- active inspector/debug sessions when that feature lands.

Detailed metrics/tracing must be opt-in where they would tax hot paths.
Cheap aggregate counters are allowed when benchmarked.

### 85.10 — Backpressure, cancellation, timeout

Required behavior:

- command queue is bounded;
- callers either await capacity or receive a structured backpressure error,
  depending on API choice;
- dropping the waiting future does not drop the isolate mid-turn;
- timeout cancels the command waiter and marks related host work cancelled;
- cancellation of a host op is best-effort, explicit, and observable in
  stats;
- future commands after cancellation/timeout still work if the isolate
  reached a consistent safepoint.

### 85.11 — Stress modes

Add stress modes:

- collect every allocation;
- collect every safepoint;
- collect after every async completion;
- collect before and after microtask checkpoints;
- force tiny command/inbox capacity to exercise backpressure;
- deterministic fake-time timer mode for tests, if feasible in this task.

## Out of scope

- Worker API and structured clone (task 92).
- Phase 2 incremental marking (task 86).
- Public web server framework integration. The handle shape must be ready
  for it, but no server is built here.
- Stable plugin ABI.
- Node compatibility semantics beyond runtime timer primitives needed by
  future Node surfaces.

## Validation gates

- [ ] `Otter` and `RuntimeHandle` satisfy `Send + Sync`; `RuntimeCore`,
  `RuntimeState`, `Interpreter`, `GcHeap`, `Gc<T>`, `Local<'gc, T>`,
  `RuntimeCx<'_>`, and `NativeCtx<'_>` do not.
- [ ] Compile-fail test proves `Value`, `Frame`, `Gc<T>`, `Local<'gc, T>`,
  `RuntimeCx<'_>`, and `NativeCtx<'_>` cannot be captured by a
  `tokio::spawn` host future.
- [ ] Tokio multi-thread test: clone `Otter`, call `tokio::spawn` from
  multiple tasks, verify runs serialize correctly and all results return.
- [ ] `run_until_promise` test: target promise resolves only while event
  loop is being driven; if loop idles with promise pending, returns a
  structured error.
- [ ] `poll_one_tick` deterministic-order tests for command, timer,
  host-op completion, microtask checkpoint, and nested microtask enqueue.
- [ ] Timer tests cover timeout, interval, cancellation, and ref/unref
  liveness.
- [ ] Dynamic import / top-level await tests use the same event-loop drive
  model as host ops.
- [ ] Cancellation test: drop the waiting future while the isolate is
  running; isolate reaches a consistent safepoint and later commands work.
- [ ] Timeout test: timeout rejects/returns `OtterError::Timeout`, no
  leaked pending host op / timer.
- [ ] Backpressure test: bounded queue refuses or awaits when full.
- [ ] Leak report test: intentionally leak a timer/host op and assert the
  diagnostic identifies id, kind, liveness, and origin.
- [ ] Activity stats test: counters for dispatched/completed/failed ops and
  ref/unref pending work update correctly.
- [ ] Stress-GC tests green in all async stress modes.
- [ ] Startup and steady-state benchmarks show the handle/event-loop layer
  does not regress sync script execution beyond an approved budget.
- [ ] `cargo test -p otter-runtime -p otter-vm -p otter-gc` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  clean.
- [ ] mdBook documents how the event loop is driven and how contributors
  write async host ops safely.

## Closing

Tick task 85 in [70-gc-master-tracker.md](./70-gc-master-tracker.md),
update ADR-0003 / ADR-0005 if public method names differ from this task,
and update mdBook event-loop / native-binding docs. Include before/after
benchmarks for sync `run_script`, async host-op roundtrip, timer wakeup,
and cold `Otter::new()` / first script execution.
