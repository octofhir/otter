# Task 12 — Boolean / Nullish / Control Flow Slice (M6)

## Goal

Land truthiness, equality, conditional branching, blocks, and the loop
families on the staging stack. After this task the interpreter can run
real branchy code with the value model from tasks `09`–`11`.

## Scope

### JS surface covered

- Literals: `true`, `false`, `null`, `undefined`.
- Logical operators: `!`, `&&`, `||`, `??`.
- Equality: `==` and `===` for the supported primitive pairs from tasks
  `09` and `11`. Pairs that involve unsupported types still produce a
  clear "feature not in this slice" diagnostic instead of silently
  returning `false`.
- Truthiness — `ToBoolean` for:
  - already-boolean values
  - `undefined` → `false`
  - `null` → `false`
  - numbers: `+0`, `-0`, `NaN` → `false`; everything else → `true`
  - strings: empty → `false`; otherwise `true`
- Conditional expressions: `cond ? a : b`.
- Blocks: `{ ... }` with proper lexical scoping for `let`/`const` (no
  `var`-style hoisting in this slice — `var` rejects with a clear
  diagnostic so `let`/`const` are the well-tested path).
- `if` / `else if` / `else`.
- `while`, `do`/`while`, `for(initializer; cond; update)`. **Not**
  `for...in`, **not** `for...of` (those land later with iterators).
- `break` and `continue` for the immediate enclosing loop. Labeled
  break/continue is deferred.
- Implicit completion value of a script — no explicit `return` outside
  a function (calls slice handles that).

### Bytecode

Add:

- `LoadTrue <reg>`, `LoadFalse <reg>`, `LoadNull <reg>` (`LoadUndefined`
  already exists from task `07`).
- `LogicalNot <dst> <src>` — implements `!` after `ToBoolean`.
- `ToBoolean <dst> <src>` — explicit coercion opcode used by branches.
- `Jump <offset>`, `JumpIfTrue <offset> <reg>`,
  `JumpIfFalse <offset> <reg>` — branch family. Forward and backward
  offsets are encoded as signed displacements relative to the next
  instruction.
- `JumpIfNullish <offset> <reg>` — supports `??`.
- Back-edge handling: every backward branch (`offset < 0`) calls the
  runtime checkpoint helper to handle interrupts, timeouts, and OOM.
- `LoopHint <kind>` — optional metadata opcode reserved for future
  inline-cache profiling; emitted by the compiler at loop headers but
  has no runtime side effect in this slice.

### Compiler integration

- Lower `if` / `else` to forward `JumpIfFalse` / `Jump`.
- Lower `while` and `do`/`while` to a labeled loop body with a back-edge
  jump and back-edge checkpoint.
- Lower `for` to an init block, a test, a body, and an update block,
  with one back-edge jump per iteration.
- Lower `&&` / `||` / `??` with short-circuit evaluation. The compiler
  emits one branch per operator, never duplicating code.
- Lower `break` / `continue` to forward jumps to the loop's break /
  continue labels. The compiler tracks an explicit label stack and
  emits a clear diagnostic if `break` / `continue` is used outside a
  loop.
- `ToBoolean` is inserted whenever a branch operand is not statically
  known boolean.

### Lexical scoping

- `let` and `const` introduce bindings in the current block.
- A binding read before its declaration produces a `TypeMismatch`
  runtime error (TDZ stand-in; the foundation plan permits a clear
  failure here without the full `Reference` machinery).
- A `const` reassignment is a compile-time error.
- Variable declarations live on the same compact frame layout as
  registers (`local0..localN`). No environment-record allocation in
  this slice.

### Interruptibility

- Back-edge checkpoints are the foundation-plan native-loop polling
  primitive. Every back-edge calls them.
- A unit test installs an interrupt handle, starts a `while (true) {}`
  fixture, fires `InterruptHandle::interrupt()` after a small delay,
  and asserts the runtime returns within the deadline with a structured
  diagnostic.

### Tests

Engine fixtures under `tests/engine/control-flow/`:

- `if-else-basic.ts`
- `if-else-if-chain.ts`
- `ternary.ts`
- `logical-and-short-circuit.ts`
- `logical-or-short-circuit.ts`
- `nullish-coalesce.ts` — `null ?? 1`, `undefined ?? 1`, `0 ?? 1`
- `truthiness-table.ts` — covers each `ToBoolean` rule
- `equality-strict-mixed.ts` — `1 === "1"` is `false`,
  `null === undefined` is `false`, `null == undefined` is `true`
- `while-loop-counter.ts`
- `do-while-runs-once.ts`
- `for-loop-with-update.ts`
- `for-loop-break-continue.ts`
- `let-const-block-scope.ts`
- `tdz-throws.ts`
- `infinite-loop-interrupt.ts` — long-running loop interrupted via the
  runtime API

Rust unit tests:

- `interrupt_handle_breaks_infinite_while`
- `back_edge_checkpoint_called_per_iteration`

Benchmarks (`crates-next/otter-vm/benches/control_flow.rs`):

- `if_branch_1m`
- `while_loop_1m_int_sum`
- `for_loop_1m_with_break`

## Out of scope

- `var` (rejected with a diagnostic).
- `for...in`, `for...of` (need iterator protocol).
- `switch`, labeled break/continue, exception handling (`try`/`catch`/
  `finally`/`throw`), generators.
- Functions, calls, closures (task `13`).

## Files / directories you may touch

- Edit / create under `crates-next/otter-vm/`,
  `crates-next/otter-compiler/`,
  `crates-next/otter-bytecode/`
- Create fixtures under `tests/engine/control-flow/`
- Add a control-flow benchmark target

## Acceptance criteria

- All `tests/engine/control-flow/*.ts` fixtures pass.
- `infinite-loop-interrupt.ts` is interrupted within the watchdog
  budget.
- `back_edge_checkpoint_called_per_iteration` confirms the checkpoint is
  invoked exactly once per back edge.
- `var` produces a clear diagnostic referencing the slice number.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine \
    --filter control-flow/
cargo bench -p otter-vm --bench control_flow -- --quick
```

## Risks

- **TDZ shortcuts.** It is tempting to skip TDZ checks. Don't —
  `tdz-throws.ts` exists to prevent silent regressions.
- **Back-edge cost.** A naïve checkpoint implementation can dominate
  loop time. The runtime checkpoint must be a cheap atomic load most of
  the time. The benchmark records the regression.
- **Short-circuit duplication.** Lowering `a && b` by emitting `b`
  twice is a known mistake. Each operand compiles once.
- **`for` initializer scoping.** `for (let i = 0; i < n; i++)` must
  scope `i` to the for body, not to the surrounding block. Add a
  fixture if this fails.

## Next task

Proceed to [`13-calls-frames-slice.md`](./13-calls-frames-slice.md).

## Status

- not started
- last update: —
- artifacts: control-flow opcodes, compiler lowering for branches /
  loops, fixtures, benchmarks
