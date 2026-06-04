---
title: "Runtime Principles"
---

Otter's runtime must be designed as a predictable tenant inside a Rust
application. Raw throughput matters, but a script that monopolizes CPU,
heap, off-heap memory, host operations, or the event loop is a runtime
bug even when it is executing valid JavaScript.

The target is a CLI-oriented and embeddable JS/TS engine that can be used
in high-load services without forcing the host application to trust user
code to be polite.

## Goals

- Minimize allocations on hot paths.
- Make every expensive resource measurable.
- Bound each VM turn with explicit budgets.
- Keep host resources deny-by-default and budgeted.
- Preserve isolate ownership: one isolate owns one VM, one runtime state,
  and one GC heap.
- Prefer many small, observable pauses over rare unbounded pauses.
- Keep instrumentation cheap when enabled and default-off when detailed.

These principles are inspired by BEAM's production runtime discipline:
small isolated execution contexts, reduction-style accounting, scheduler
visibility, per-process memory pressure, and explicit treatment of large
off-heap data. Otter should borrow the resource model, not Erlang's
language semantics or actor API.

## Non-Goals

- Do not add actor semantics to JavaScript.
- Do not make all JS objects immutable or persistent.
- Do not make REPL design part of this runtime contract. REPL ergonomics
  are a separate product design topic.
- Do not add a JIT before the interpreter has compact bytecode, inline
  caches, and reliable resource accounting.

## Resource Budgets

Every runtime turn should be able to run under a `RuntimeBudget` policy.
The exact public API can change, but the policy needs these dimensions:

- reductions or instruction units;
- allocation bytes and allocation count;
- external/off-heap bytes;
- host operation enqueue count;
- microtask drain count;
- maximum contiguous turn duration;
- optional stack depth and recursion limits.

The default policy is observational: it records exceedances for these
dimensions without changing JavaScript-visible completion. Embedders can opt
into hard rejection, which returns a structured `BUDGET_EXCEEDED` runtime
diagnostic at VM checkpoints. Cooperative yield and resumable scheduling remain
future work.

Budget exhaustion must not be modeled as an arbitrary internal crash. The
VM should distinguish:

- normal completion;
- thrown JavaScript exception;
- host/runtime failure;
- cooperative yield because the turn spent its budget;
- hard budget rejection when policy says yielding is not allowed.

The direct CLI can usually continue a yielded turn immediately. Embedded
runtime handles should be able to reschedule yielded work so other inbox
messages, timers, or host completions can make progress.

## Reductions

Otter should count execution in reduction-like units. A reduction is not
required to map one-to-one to a bytecode instruction; the useful property
is stable accounting at low overhead.

Initial charging rules:

- simple register bytecodes are cheap;
- calls, native calls, property slow paths, proxy traps, eval, module
  loading, and iterator/async machinery charge more;
- loop backedges and basic-block entries are preferred accounting points
  once compact bytecode has block metadata;
- allocation charges both reductions and bytes.

The dispatch loop must not perform expensive accounting on every opcode if
that becomes measurable. Use block-level or backedge counters where the
bytecode format supports it.

## Allocation Discipline

Hot execution structures must be compact and allocation-free where
possible:

- execution bytecode must not store per-instruction `Vec` operands;
- property names in hot paths should be atom ids, not allocated strings;
- native calls should receive borrowed argument views, not owned vectors;
- call frames should keep hot state separate from cold exception/async
  state;
- ordinary objects and arrays should allocate in young space once all roots
  and containers are fixup-safe.

Mutable builders are allowed during compile/link/bootstrap, but published
execution products must be frozen, compact, and deterministic. Treat this
like a transient-builder discipline:

```text
mutable builder -> frozen execution product -> shared read-only use
```

Do not leak builder-only structures such as `Rc<RefCell<_>>`,
`HashMap<String, ...>`, or `Vec<Operand>` into runtime hot paths.

## External Memory

Memory outside GC cells must be accounted as part of runtime pressure.
This includes:

- `ArrayBuffer` and `SharedArrayBuffer` backing stores;
- string backing storage and rope/flattening payloads;
- retained source text;
- bytecode cache blobs;
- JSON parse source slices;
- native module buffers and host resources.

External memory should be able to trigger GC or budget rejection before
the host process becomes memory pressured. Large/off-heap data needs
separate counters because GC heap usage alone is not enough to describe
resident memory. Shared backing stores that can be dropped away from the
mutator thread must report releases through heap-owned shared accounting state
instead of mutating isolate-local heap counters directly.

## Host Operations

Host work is part of the resource model.

Native and hosted APIs must:

1. validate permissions on the isolate thread;
2. charge the budget before opening resources or enqueueing work;
3. copy owned host data out of VM values;
4. run host work without VM/GC handles;
5. post owned completion data back to the isolate;
6. settle JS promises or callbacks on a later mutator turn.

CPU-heavy native work must either be demonstrably fast, budget-charged, or
moved to a host worker lane. Long native work must not block isolate
responsiveness.

## Event Loop Fairness

Microtasks are VM work and must be budgeted. A recursive
`queueMicrotask` chain or promise reaction storm must not run forever in
one checkpoint.

Runtime drive modes should remain deterministic:

- one tick;
- run until idle;
- run until promise;
- shutdown.

Budgeted execution adds another observable state: yielded but resumable.
The event-loop layer should be able to requeue yielded JS work without
losing pending timers, host completions, interrupts, or diagnostics.

## Observability

The runtime must expose cheap aggregate counters suitable for tests and
production diagnostics:

- reductions executed;
- forced yields;
- max contiguous VM turn duration;
- allocations and allocated bytes;
- external/off-heap bytes;
- host-operation enqueues from VM work;
- GC count, GC time, reclaimed bytes;
- pending commands, timers, host ops, dynamic module jobs, and microtasks;
- cancelled or rejected host work;
- budget rejections.

Detailed traces and profiles should stay opt-in and use standard formats
where practical. The default product path should not maintain expensive
per-op telemetry.

## Implementation Rules

- New runtime work that can consume CPU, heap, off-heap memory, host
  resources, or event-loop turns must define how it is budgeted.
- New external memory owners must use explicit accounting.
- New native APIs must not retain VM values across async boundaries.
- New bytecode/runtime hot-path structures must separate builder state from
  frozen execution state.
- New diagnostics/profiling fields should be machine-readable.
- Changes to budget, yield, host-op, or external-memory behavior must update
  this page in the same patch.

## Acceptance Tests

Runtime-resource work is incomplete until it has tests or benchmarks for:

- CPU-heavy loop under a small reduction budget;
- recursive microtask chain under a microtask budget;
- timer or host completion competing with CPU-heavy JS;
- allocation-heavy object/array workload under heap budget;
- large `ArrayBuffer` or string workload under external-memory budget;
- cancellation or rejection of host work without leaked pending promises.

## See Also

- [Engine Architecture](architecture.md)
- [Event Loop And Async Boundary](event-loop.md)
- [GC API](gc-api.md)
- [Startup Performance](../performance/startup.md)
