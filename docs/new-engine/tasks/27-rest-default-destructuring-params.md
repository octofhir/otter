# Task 27 — Rest, default, and destructuring parameters

## Goal

Support `function f(a, b = 5, ...rest)` and the array / object
destructuring forms in parameter positions.

## Scope

- Default parameter values (`b = expr`) — `expr` is evaluated lazily
  per call when the corresponding argument is `undefined`.
- Rest parameters (`...rest`) — bound to a fresh array of trailing
  arguments.
- Array destructuring (`[a, b, ...t]` in params or `let` binding).
- Object destructuring (`{ x, y: alias, z = 1, ...rest }`).
- Compiler reuses `compile_function`'s parameter loop and emits
  the destructuring lowering as a sequence of property /
  iterator-protocol calls (depends on task 25 for the iterator
  protocol).

## Out of scope

- Nested patterns deeper than two levels — first pass keeps the
  recursive lowering general but prefers simple shapes for
  fixtures.
- Renaming through computed keys.

## Files / directories you may touch

- `crates-next/otter-compiler/`
- `tests/engine/calls/destructuring/`

## Acceptance criteria

- `function f(a, b = 5) { return a + b; }; f(1)` returns `6`.
- `function f(...rest) { return rest.length; }; f(1,2,3)` returns
  `3`.
- `function f({ x, y = 9 }) { return x + y; }; f({ x: 1 })`
  returns `10`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter calls/destructuring/
```

## Risks

- Object destructuring evaluates default values lazily and
  property keys in source order — easy to get backwards.

## Status

- not started
