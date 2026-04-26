# Task 25 — Iterator protocol and `for…of`

## Goal

Implement the standard iterator protocol (`@@iterator` / `next` /
`{ value, done }`), `for…of` syntax, and spread (`...x`) in calls
and array literals.

## Scope

- A well-known symbol "iterator" — for the foundation, use a
  reserved string key `"@@iterator"` until task 37 adds real
  `Symbol`.
- `Array.prototype[@@iterator]` returns a fresh iterator object.
- `String.prototype[@@iterator]` iterates code units.
- Compiler lowering for `for (let x of expr) { ... }` to:
  `tmp = expr[@@iterator](); loop { let r = tmp.next(); if (r.done)
  break; let x = r.value; <body> }`.
- Spread:
  - `[...arr, 4, 5]` — append iterator items into the new array.
  - `f(...arr)` — call with iterator items as args.

## Out of scope

- `for await…of`.
- Generator functions — separate task once exceptions and `yield`
  state machines are designed.
- `Set`/`Map` iteration (their iterators land with task 38).

## Files / directories you may touch

- `crates-next/otter-vm/`
- `crates-next/otter-compiler/`
- `tests/engine/iterators/`

## Acceptance criteria

- `for (let x of [1, 2, 3]) { ... }` walks 1, 2, 3.
- `for (let c of "abc") { ... }` walks "a", "b", "c".
- `[1, ...[2, 3], 4]` returns `[1, 2, 3, 4]`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter iterators/
```

## Risks

- Without real `Symbol`, the temporary `"@@iterator"` string key is
  observable to user code — document and replace in task 37.

## Status

- not started
