# Event Loop And Async Boundary

Otter's public runtime is handle-first and async-friendly, but one isolate
still owns one VM, one runtime state, and one GC heap. The public handle may
be `Send + Sync`; the isolate internals are not.

The production event-loop boundary landed in task 85. Deno's
`JsRuntime` shape is the closest reference: the runtime itself stays
local to one isolate, while embedders drive it with one-tick and
run-to-idle style APIs. Boa's job model is the smaller ECMA-262
reference: promise, timeout, native async, and generic jobs run only
when no execution context is active and each job runs to completion.

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

A runtime turn runs JS work on the mutator, performs a microtask checkpoint,
then folds host completions into the runtime inbox according to the selected
drive mode.

Tokio-specific state belongs in `TokioEventLoop`. Runtime handles carry
owned command payloads, timer tokens, and completion records; they do not
hold VM values, GC handles, or executor locks.

## Drive Modes

The runner should support deterministic drive modes:

- `poll_one_tick`: process at most one event-loop turn and checkpoint;
- `run_until_idle`: run referenced work until the runtime is idle;
- `run_until_promise`: drive until a target promise settles or the loop
  becomes idle with that promise still pending;
- `run_until_command`: drive until a command completion is delivered;
- `shutdown`: cancel or drain, then report leaks.

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

Host operations follow this concrete pattern:

```rust
let handle = otter.handle().clone();
handle.spawn_host_op(RuntimeLiveness::Ref, Box::pin(async move {
    // Owned host data only. No VM/GC handles here.
    HostOpCompletion {
        id: 0, // RuntimeHandle assigns the final id before posting.
        kind: "example".to_string(),
        result: Ok("done".to_string()),
    }
}));
```

The isolate runner receives the completion as a runtime inbox message on a
later turn, then performs the JS-side resolution/checkpoint work on the
mutator thread.

## Liveness And Diagnostics

Timers and host ops have ref/unref liveness. Referenced work keeps
`run_until_idle` alive; unreferenced work may finish if the loop is already
being driven but must not keep the runtime alive by itself.

Contributor tests should be able to inspect activity stats: pending
commands, timers, host ops, dynamic module jobs, microtasks, cancellations,
timeouts, and leaked work at shutdown.

`RuntimeHandle::activity_stats()` exposes cheap aggregate counters for this
purpose. Detailed tracing should stay opt-in so native dispatch and script
startup keep their steady-state cost.
