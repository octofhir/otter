# Task 23 — `this` binding and method calls

## Goal

Add a `this` binding to call frames, lower `obj.method()` to a
method-call shape that passes the receiver, and implement
`Function.prototype.{bind, call, apply}`.

## Scope

- `Frame` gains `this_value: Value`.
- New opcode `CallMethodValue <dst> <obj> <name_const> <argc>
  <args...>` — dispatches `obj.<name>(...args)` with `obj` as
  `this`. Reuses the property-load path; throws `TypeError` when
  the property is not callable.
- New opcode `CallWithThis <dst> <callee> <this> <argc> <args...>`
  — used by `bind`/`call`/`apply` lowering.
- Arrow functions inherit the enclosing function's `this` (they
  capture it as an implicit upvalue, building on task 22).
- Foundation rule: at module top level, `this` is `Value::Undefined`.

## Out of scope

- `globalThis` global object.
- Strict-mode subtleties beyond the foundation default ("strict").
- `new` (separate task once classes / constructors land).

## Files / directories you may touch

- `crates-next/otter-bytecode/`
- `crates-next/otter-vm/`
- `crates-next/otter-compiler/`
- `tests/engine/methods/`

## Acceptance criteria

- `({ x: 1, get(): any { return this.x; } }).get()` returns `1`.
- `function f() { return this.v; } f.call({ v: 7 })` returns `7`.
- `f.apply({ v: 9 }, [])` returns `9`.
- `f.bind({ v: 5 })()` returns `5`.
- Arrow `this` capture works:
  `function outer() { return () => this; }`
  called with `.call(obj)` returns `obj`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter methods/
```

## Risks

- `bind` returns a "bound function" — foundation can store the
  bound `this` and partially-applied args in a small wrapper value
  variant (`Value::BoundFunction`).

## Status

- not started
