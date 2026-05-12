# Event Loop And Async Boundary

Otter's public runtime is handle-first and async-friendly, but one isolate
still owns one VM, one runtime state, and one GC heap. The public handle
may be `Send + Sync`; the isolate internals are not.

The default product path is async-first. CLI execution starts in an async
`main` and awaits the public `Otter`/`RuntimeHandle` stack directly.
Blocking wrappers exist only as sync-caller conveniences. Blocking does
not mean a separate synchronous runtime: the same event-loop-capable
isolate runner must remain available for timers, host ops, dynamic
modules, workers, and future async Web APIs.

The production event-loop boundary follows Deno's `JsRuntime` shape: the
runtime itself stays local to one isolate, while embedders drive it with
one-tick and run-to-idle style APIs. Boa's job model is the smaller
ECMA-262 reference: promise, timeout, native async, and generic jobs run
only when no execution context is active and each job runs to completion.

## Runtime Layers

The intended shape is:

```text
Otter              // public facade; Clone + Send + Sync
  -> RuntimeHandle // bounded command/completion API
    -> IsolateRunner
      -> RuntimeCore / Interpreter / RuntimeState / GcHeap // !Send + !Sync
```

Tokio is the default scheduler in `otter-runtime`, but VM crates must not
import Tokio types.

## Queues

Do not overload one queue for all async work:

- VM microtask queue: Promise reactions, `queueMicrotask`, async-function
  resume, and `await` resume.
- Runtime inbox: commands, host-op completions, timers, dynamic module
  completion, interrupts, inspector/debug events, and shutdown.

A runtime turn runs JS work on the mutator, performs a microtask
checkpoint, then folds host completions into the runtime inbox according
to the selected drive mode.

Runtime turns are budgetable. A CPU-heavy script, promise reaction storm,
or long host callback chain must be able to yield or fail according to the
resource policy described in [Runtime Principles](runtime-principles.md).

Microtask checkpointing is VM work. Promise reactions, `queueMicrotask`,
and async-function resumes run only after the current JS execution context
unwinds and before the runtime turn is considered complete.

Tokio-specific state belongs in the runtime's internal event-loop and
host-service layer. Runtime handles carry owned command payloads and
settlement messages; timer callbacks re-enter the isolate by opaque timer
token. Handles do not hold VM values, GC handles, or executor locks.

## Drive Modes

The runner should support deterministic drive modes:

- `poll_one_tick`: process at most one event-loop turn and checkpoint;
- `run_until_idle`: run referenced work until the runtime is idle;
- `run_until_promise`: drive until a target promise settles or the loop
  becomes idle with that promise still pending;
- `run_until_command`: drive until a command completion is delivered;
- `shutdown`: cancel or drain, then report leaks.

Budgeted execution adds a resumable yielded state. A yielded turn remains
owned by the isolate runner and can be requeued without losing timers,
host completions, interrupts, diagnostics, or pending microtasks.

## Async Host Ops

Native async APIs must split at the runtime boundary:

1. validate arguments and permissions on the isolate thread;
2. copy owned host data;
3. create a pending promise / operation id;
4. run Rust async work on the event loop without VM references;
5. post an owned completion back to the isolate;
6. resolve or reject the promise on a later mutator turn;
7. run the microtask checkpoint.

Never move `RuntimeCx`, `NativeCtx`, `Value`, `Frame`, `Gc<T>`,
`Local<'gc, T>`, or handle scopes into a Rust future.

Host operations should be exposed through narrow runtime-owned services or
typed inbox messages. The isolate runner receives only owned completion
data on a later turn, then performs the JS-side resolution/checkpoint work
on the mutator thread.

Cancellation and backpressure are runtime-handle concerns. Dropping or
aborting host work must not leave a JS promise in an untracked state:
record the operation id, decrement liveness counters, and settle or report
the pending JS work on the isolate turn that observes cancellation.

## Liveness And Diagnostics

Timers and host ops have ref/unref liveness. Referenced work keeps
`run_until_idle` alive; unreferenced work may finish if the loop is already
being driven but must not keep the runtime alive by itself.

Use ref/unref deliberately:

- `Ref` for work that the user can observe and that should keep
  `run_until_idle` alive;
- `Unref` for background diagnostics or cache cleanup that may complete
  opportunistically but must not prevent idle shutdown.

Contributor tests should be able to inspect activity stats: pending
commands, timers, host ops, dynamic module jobs, microtasks, cancellations,
timeouts, and leaked work at shutdown.

`RuntimeHandle::activity_stats()` exposes cheap aggregate counters for this
purpose. Detailed tracing should stay opt-in so native dispatch and script
startup keep their steady-state cost.
