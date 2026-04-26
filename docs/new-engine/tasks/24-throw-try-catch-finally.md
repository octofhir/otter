# Task 24 — `throw` / `try` / `catch` / `finally`

## Goal

Add user-throwable exceptions and structured `try`/`catch`/`finally`
handling that unwinds frames cleanly.

## Scope

- New opcode `Throw <src>` — throws `r<src>` as the current
  exception.
- New opcode `EnterTry <catch_offset> <finally_offset>` — pushes a
  try-handler entry onto a per-frame handler stack.
- New opcode `LeaveTry` — pops the most recent handler.
- Compiler lowers `try { A } catch (e) { B } finally { C }` with
  the usual prologue / handler entries / epilogue, using forward-
  jump patching for the success path through `finally` and the
  exception path that lands in `catch`.
- Throws walk the handler stack; if no handler is found in the
  current frame, the frame is popped and the search continues in
  the caller. If the frame stack empties without a handler, the
  exception surfaces as `OtterError::Runtime { diagnostic }` with
  `code = "UNCAUGHT"` and `frames` populated (depends on task 16).
- `Error` constructor and basic `Error.prototype.{message, name}`
  so user code can `throw new Error("msg")`. Foundation subset:
  one base `Error`, no subclasses yet.

## Out of scope

- Subclasses (`TypeError` / `RangeError` …) as JS values that the
  user can construct.
- `try { ... } catch { ... }` (catch without binding) — separate
  follow-up.

## Files / directories you may touch

- `crates-next/otter-bytecode/`
- `crates-next/otter-vm/` (handler stack on `Frame`).
- `crates-next/otter-compiler/`
- `tests/engine/exceptions/`

## Acceptance criteria

- `try { throw new Error("boom"); } catch (e) { e.message }`
  returns `"boom"`.
- `finally` runs on both normal completion and exception unwinding.
- Uncaught exception escapes as `OtterError::Runtime` with a non-
  empty `frames` chain.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter exceptions/
```

## Risks

- `finally` re-throw semantics — if `finally` itself throws, that
  exception replaces the in-flight one. Document the rule clearly
  in the compiler module docstring.

## Status

- not started
