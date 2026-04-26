# Task 35 — `async` functions and `await`

## Goal

Implement `async function` declarations / expressions and the
`await` operator on top of the promise machinery from task 34.

## Scope

- Compiler lowers an `async` function to a state-machine
  representation: each `await` becomes a suspension point that
  records the continuation pc and saves register state.
- Runtime: a suspended async-call frame is parked; when the awaited
  promise settles, the frame resumes from the saved pc with the
  result (or rejection rethrown into the function).
- `await` of a non-promise resolves immediately with the value.
- Top-level `await` (in modules) is **out of scope** until the
  module slice ships.
- `async` arrow functions follow the same lowering.

## Out of scope

- `for await…of`.
- Async generators (`async function*`).

## Files / directories you may touch

- `crates-next/otter-bytecode/`
- `crates-next/otter-vm/`
- `crates-next/otter-compiler/`
- `tests/engine/async/await/`

## Acceptance criteria

- `async function f() { return 7; }; f().then((v) => log(v))`
  drains via microtask and logs `7`.
- `async function f() { let x = await Promise.resolve(1); return
  x + 1; }; f().then((v) => log(v))` logs `2`.
- A throw inside `async f()` surfaces as a rejected promise.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter async/await/
```

## Risks

- Saving / restoring register state across `await` requires
  careful frame layout; lock the convention in the dispatcher
  module docstring.

## Status

- not started
