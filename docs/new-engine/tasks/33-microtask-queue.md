# Task 33 — Microtask queue and `queueMicrotask`

## Goal

Add a per-runtime microtask queue and the `queueMicrotask(fn)`
global so promise infrastructure has a concrete drain point.

## Scope

- `Runtime` owns a `VecDeque<Microtask>` (each task is an enqueued
  closure-less function-value plus its arg list).
- New public API: `Runtime::run_microtasks()` drains the queue
  until empty (or a hard iteration cap is hit).
- Built-in `queueMicrotask(fn)` enqueues a job.
- Each `Otter::run_*` / `Runtime::run_*` automatically drains the
  microtask queue **after** the script returns.
- Diagnostics: a microtask that throws bubbles through
  `OtterError::Runtime` with frames pointing at the microtask's
  registration site (best-effort).

## Out of scope

- Macrotask queue, `setTimeout`, `setInterval`.
- `process.nextTick`.
- Async event-loop integration.

## Files / directories you may touch

- `crates-next/otter-runtime/`
- `crates-next/otter-vm/`
- `tests/engine/async/`

## Acceptance criteria

- `let log = []; queueMicrotask(() => log.push("a")); log.push("b");
  log.join(",")` returns `"b,a"`.
- A microtask that throws surfaces a structured diagnostic.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter async/
```

## Risks

- The drain-after-script rule must not be skipped on error paths;
  document the contract.

## Status

- not started
