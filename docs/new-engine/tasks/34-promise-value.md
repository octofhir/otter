# Task 34 — `Promise` value

## Goal

Implement the `Promise` constructor and prototype methods so
`new Promise((resolve, reject) => ...)` works and `then` / `catch`
/ `finally` chain correctly.

## Scope

- `Value::Promise(JsPromise)` with three states: pending, fulfilled,
  rejected.
- `Promise.prototype.{then, catch, finally}`.
- `Promise.resolve(v)`, `Promise.reject(e)`, `Promise.all`,
  `Promise.race`.
- Chained `then` callbacks run as microtasks (depends on task 33).
- Unhandled rejection: surfaces as a runtime diagnostic the next
  microtask cycle (foundation behavior; later slices may add a
  hook).

## Out of scope

- `Promise.allSettled`, `Promise.any` — follow-up.
- `async`/`await` — task 35 builds on this.
- Cancellation tokens.

## Files / directories you may touch

- `crates-next/otter-vm/` (promise module).
- `crates-next/otter-runtime/` (global registration).
- `tests/engine/async/promise/`

## Acceptance criteria

- `Promise.resolve(7).then((v) => v + 1)` settles to `8` (drained
  before the script returns).
- Rejection propagates through `catch`.
- `Promise.all([Promise.resolve(1), Promise.resolve(2)])` settles
  to `[1, 2]`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter async/promise/
```

## Risks

- Promise resolution procedure (handling thenables) — pin the
  iterative algorithm in the module docstring.

## Status

- not started
