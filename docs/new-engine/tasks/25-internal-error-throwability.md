# Task 25 — Internal-error → throwable Error conversion

Follow-up to the §19.3 / §20.5 error class hierarchy that landed
under [`41-spec-gap-audit.md` row #57](./41-spec-gap-audit.md):
the registry + opcodes + compiler interception are in, so user
code can `throw new TypeError(...)`, `e instanceof TypeError`,
and read `e.name` / `e.message`. What remains is wiring every
*internal* runtime-raised `VmError` into a real Error instance
so `try { ... } catch (e) { e instanceof TypeError }` works for
implicit failures (e.g. calling a non-callable, indexing
undefined, BigInt mixing).

## Surface

For every catchable [`VmError`] variant, build the matching
[`ErrorKind`] instance through [`ErrorClassRegistry::make_instance`]
and route through [`Interpreter::unwind_throw`] instead of
propagating the raw [`VmError`]:

- `TypeMismatch`, `NotCallable`, `UnknownIntrinsic` → `TypeError`.
- `OutOfMemory`, `StackOverflow` → `RangeError`.
- `TemporalDeadZone` → `ReferenceError`.
- `InvalidRegExp` → `SyntaxError`.
- `JsonError` → `SyntaxError`.

Compiler-bug variants (`MissingReturn`, `InvalidOperand`) and host
cancellation (`Interrupted`) keep their fatal escape path since
they don't represent JS-observable conditions.

## Mechanics

The cleanest hook point is the dispatch loop's `?`-propagation
seam: catch `VmError` after each instruction's execution, ask
`error_kind_of(&err)` for the matching kind (returning `None`
for fatal variants), build the instance, call `unwind_throw`,
re-enter the loop. If `unwind_throw` walks the entire stack
without a handler, raise `VmError::Uncaught` (with the new
formatting — `"<Name>: <message>"`).

## Out of scope

- ES2022's `Error.cause` (second `options` argument). The
  compiler currently rejects more than one argument; lifting
  that gate ships alongside `cause` propagation.
- `Error.captureStackTrace` (V8 extension) — not in ECMA-262.

## Status

Open.
