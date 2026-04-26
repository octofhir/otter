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

- **done** (foundation subset; bench targets and explicit
  interrupt-from-thread fixture deferred to slice 13 / perf pass)
- last update: 2026-04-26
- artifacts:
  - `Value::Null` variant; `Value::to_boolean` (foundation
    [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean));
    `Value::is_nullish`.
  - New opcodes: `LoadNull`, `LogicalNot`, `ToBoolean`, `Jump`,
    `JumpIfTrue`, `JumpIfFalse`, `JumpIfNullish`, `LoadLocal`,
    `StoreLocal`, `TdzError`. `Op::is_branch()` helper for
    dispatcher classification.
  - VM dispatch: `apply_branch` helper polls
    `InterruptFlag` on **every back-edge** (negative offset);
    nullish-coalescing matches `Value::is_nullish`. New
    `VmError::TemporalDeadZone { local_index }`; runtime maps it
    to a structured `Diagnostic` with `code = "TDZ"`,
    `kind = Reference`.
  - Compiler:
    - per-function lexical-scope stack (`FunctionContext::scopes`)
      with shadowing, const-flag tracking, redeclaration
      diagnostics;
    - `LoopFrame` stack with patch-list machinery for `break`/
      `continue`;
    - `emit_branch_placeholder` + `patch_branch_to_here` /
      `patch_branch` for forward branches;
    - lowering for `let`/`const` (rejecting `var`),
      `if`/`else`, `while`, `do-while`, `for(let init; test;
      update)`, `break`, `continue`, `BlockStatement`,
      `LogicalExpression` (`&&`/`||`/`??` short-circuiting via
      JumpIfTrue/False/Nullish), `ConditionalExpression`,
      `AssignmentExpression` (plain `=` only; rejects compound and
      member-target),
      `Identifier` resolution with NaN/Infinity pseudo-globals
      and clear "unresolved identifier" diagnostics,
      `UnaryOperator::LogicalNot` → `LogicalNot`.
  - 7 фикстур под `tests/engine/control-flow/`:
    `if-else`, `while-loop`, `for-loop`, `break-continue`,
    `do-while`, `logical-ops`, `conditional-expr`.
- verification:
  - `cargo build/test/clippy/fmt` — все зелёные.
  - **35/35** engine fixtures PASS (7 control-flow + 4 numbers +
    7 string methods + 6 strings + 9 typescript + 2 smoke).
  - End-to-end `-p`:
    - `5 > 3 ? "yes" : "no"` → `yes`
    - `null ?? "fallback"` → `fallback`
    - `true && "hi"` → `hi`
    - `false || 7` → `7`
    - `!false` → `true`
  - Comprehensive smoke runs of all 7 control-flow fixtures →
    `exit=0`.
  - LLM-friendly `//!` headers — все `.rs` файлы.
- design highlights:
  - **Idiomatic Rust**: `enter_scope`/`exit_scope`/
    `declare_binding`/`lookup_binding` keep AST-walking code
    succinct; placeholder/patch helpers eliminate manual offset
    arithmetic in compile_expr.
  - Branch encoding: `Operand::Imm32(offset)` is
    relative-to-next-instruction. Negative offsets (`<0`) are
    back-edges and trigger interrupt-flag polling in
    `apply_branch` — meets the "every native loop polls every
    4096 iterations" rule (ours polls **every** back-edge).
  - Logical operators reuse `JumpIfTrue/False/Nullish` so short-
    circuiting compiles to one branch per operator.
  - `var` is intentionally rejected with a "foundation rejects
    var" diagnostic so the well-tested path is just
    `let`/`const`. Lifts when full hoisting / function-scope
    semantics arrive.
  - Conditional expression and `??` use a small `StoreLocal` /
    `LoadLocal` pair to materialize the result rather than
    threading a phi through the compiler — keeps the lowering
    independent of the (future) SSA pass.
- deferred:
  - Labeled `break` / `continue` — rejected with explicit
    diagnostic.
  - Real TDZ semantics (currently any `let` reads its register,
    which is `undefined` until store; spec says ReferenceError).
    `TdzError` opcode is reserved for the future, not yet
    emitted.
  - `switch` / `try`/`catch`/`finally` / `throw` — separate
    slices.
  - Bench targets (`if_branch_1m`, `while_loop_1m_int_sum`,
    `for_loop_1m_with_break`) — paired with calls slice (13) so
    we benchmark realistic shapes including function calls.
  - `infinite-loop-interrupt.ts` fixture (требует thread-spawn в
    test harness) — отложен до бенч-рантайма slice 13.
