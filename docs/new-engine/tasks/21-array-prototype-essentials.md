# Task 21 — Array.prototype essentials

## Goal

Wire the basic `Array.prototype.*` methods through the existing
intrinsic-table machinery so common loops work.

## Scope

- New `IntrinsicReceiver::Array` variant.
- Array-prototype intrinsics (registered via the `intrinsics!`
  macro):
  - `push`, `pop`, `shift`, `unshift`.
  - `slice`, `concat`, `join`.
  - `forEach`, `map`, `filter`, `reduce`.
  - `indexOf`, `includes`.
- Compiler lowering: `arr.method(args...)` dispatches through the
  same opcode family as string methods (a new
  `Op::CallArrayMethod` mirrors `Op::CallStringMethod`).
- Callbacks for `forEach` / `map` / `filter` / `reduce` invoke the
  user function via the existing `Op::Call` machinery.

## Out of scope

- `Array.from`, `Array.of`, `Array.isArray` — separate tasks.
- `find`, `findIndex`, `flat`, `flatMap`, `at`, `entries`,
  `keys`, `values` — follow-up sub-task.
- Sparse-array-aware iteration.

## Files / directories you may touch

- `crates-next/otter-vm/` (`array_prototype` module).
- `crates-next/otter-bytecode/`
- `crates-next/otter-compiler/`
- `tests/engine/arrays/methods/`

## Acceptance criteria

- `[1, 2, 3].map((x) => x * 2)` returns `[2, 4, 6]`.
- `[1, 2, 3].reduce((a, b) => a + b, 0)` returns `6`.
- `[1, 2, 3].push(4)` returns `4` and mutates the array.
- Engine suite green; new fixture set under `methods/`.

## Verification commands

```bash
cargo run -p otter-cli -- -p '[1, 2, 3].map((x) => x * 2)'
cargo run -p otter-cli -- test --suite engine --filter arrays/methods/
```

## Risks

- Callback shape: `forEach((value, index, array) => ...)` — make
  sure the callback receives the right number of args.
- Result object identity — `slice` returns a fresh array; `map`
  returns a fresh array of the same length.

## Status

- not started
