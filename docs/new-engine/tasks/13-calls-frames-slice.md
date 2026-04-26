# Task 13 — Calls and Frames Slice (M7)

## Goal

Add functions, fixed-arity calls, and the production-grade frame model
to the staging stack. Calls must not allocate per-invocation `Vec`s or
heap blocks for common shapes.

This is the last task in the foundation batch. After it lands, the next
batch (objects, shapes, arrays, builtins, conformance) can be planned.

## Scope

### JS surface covered

- Function declarations: `function f() {}`
- Function expressions: `const f = function() {}`
- Arrow functions **without** captured environment lookups in this
  slice — only arrow expressions that touch parameters and locals.
  Closures over outer variables are deferred to a follow-up slice;
  the compiler emits a clear diagnostic if it cannot prove an arrow
  captures only its own parameters.
- Call expressions with fixed arity: `f(a, b)`.
- `return <expr>` and bare `return`.
- Stack-depth limit returning a catchable diagnostic (foundation plan §M7).
- Source spans on stack traces.

Not yet:

- Closures over outer variables, `let`/`const` capture, free variable
  lookup. Parameters and locals only.
- `arguments` object.
- Default parameters, rest parameters, destructuring parameters.
- `this` binding beyond the placeholder used in this slice.
- Constructors, `new`, prototypes.
- Method shorthand on objects (no objects yet).
- Tail calls / generators / async.

### Frame model

`CallFrame` layout (compact, cache-conscious; see also the existing
project notes in `MEMORY.md` for the active-stack layout — the staging
stack mirrors that design rather than copying it):

- `function_id: u32` — index into the bytecode container's function
  table.
- `pc: u32` — current program counter.
- `register_base: u32` — offset into the shared `registers: Vec<Value>`
  buffer.
- `local_count: u16`, `scratch_count: u16` — register window split.
- `argc: u16` — number of declared arguments at the call site.
- `flags: CallFrameFlags(u8)` — packed bits: `is_construct`,
  `this_initialized`. Reserved space for future flags.
- `return_register: u16` — caller register that receives the result.
- A single `registers: Vec<Value>` buffer is shared across frames
  (register window pattern, like the active stack). New frames extend
  the buffer and pop on return.

A `Vec<Value>` is **not** allocated per call. The shared buffer grows
geometrically; arguments are written into the new window directly by
the caller before `Call` dispatches.

### Bytecode

Add:

- `LoadFunction <reg> <function_id>` — loads a function value pointing
  at the bytecode-container function entry.
- `Call <dst> <callee_reg> <argc>` — arguments live in
  `<callee_reg + 1> .. <callee_reg + 1 + argc>` (register-range fast
  path; no allocation).
- `Return <reg>`, `ReturnUndefined` — pop the frame and write the
  result into the caller's `return_register`.
- `EnterFunction` — emitted at function start; allocates the register
  window in the shared buffer.
- `CheckStackDepth` — explicit opcode emitted at every function entry
  that increments a counter and triggers a structured diagnostic if the
  configured limit is exceeded. The limit is configurable via
  `RuntimeBuilder::with_max_stack_depth`.

The `CallMethod` opcode from task `10` is **not** changed by this
task. Generic call dispatch is unified under `Call`; method dispatch on
known intrinsics keeps using `CallMethod`. A future objects slice
unifies these paths once shape-based dispatch is available.

### Compiler integration

- Emit `LoadFunction` for function declarations and expressions.
- Lower call expressions to `Call <dst> <callee_reg> <argc>` with
  arguments staged into the consecutive registers above the callee.
- Lower `return` to `Return` / `ReturnUndefined`.
- Function declarations are hoisted within their containing block per
  the spec subset; `let`/`const` ordering rules from task `12` still
  apply.

### Stack traces

- The `RuntimeError::Runtime(Diagnostic)` type carries a chain of
  `StackFrame { function_name, module, span }` entries assembled from
  the call frame stack at the point of failure.
- The CLI formatter renders the frames with code-frame snippets where
  the source is available.

### Tests

Engine fixtures under `tests/engine/calls/`:

- `call-fixed-arity.ts` — `function add(a, b) { return a + b; }`
- `return-undefined-implicit.ts` — falling off the end returns
  `undefined`
- `nested-calls.ts`
- `recursion-fibonacci-small.ts` — `fib(20)` runs without stack
  overflow under default depth limit
- `stack-overflow-throws.ts` — recursion depth above the configured
  limit produces a catchable diagnostic; the fixture asserts the
  diagnostic kind.
- `extra-arguments-ignored.ts`
- `missing-arguments-undefined.ts`
- `arrow-no-capture.ts`
- `arrow-with-capture-rejected.ts` — captures an outer variable;
  the compiler rejects with a "feature deferred" diagnostic.
- `stack-trace-shape.ts` — throws a runtime error from a nested call
  and asserts the diagnostic's frame chain has the expected
  function names and source spans (golden file).

Rust unit tests:

- `call_no_per_invocation_alloc` — calls `f(1, 2)` 10 000 times and
  asserts the runtime allocation counter delta is zero.
- `register_window_growth_amortized_constant` — records the
  amortized growth of the shared register buffer.

Benchmarks (`crates-next/otter-vm/benches/calls.rs`):

- `call_overhead_empty_function`
- `call_overhead_two_args`
- `recursion_fib_25`

## Out of scope

- Closures, `arguments`, rest/default/destructuring parameters,
  generators, async functions, tail calls, `new`/constructors,
  method/property accessor calls, `bind`/`call`/`apply`.
- The unified call/method dispatch path (lands with the objects slice).
- `this` semantics beyond a placeholder undefined value.

## Files / directories you may touch

- Edit / create under `crates-next/otter-vm/`,
  `crates-next/otter-compiler/`,
  `crates-next/otter-bytecode/`,
  `crates-next/otter-runtime/`
- Create fixtures under `tests/engine/calls/`
- Add `crates-next/otter-vm/benches/calls.rs`

## Acceptance criteria

- All `tests/engine/calls/*.ts` fixtures pass.
- `call_no_per_invocation_alloc` confirms zero allocation for a
  10 000-iteration call loop with two arguments.
- `recursion-fibonacci-small.ts` runs `fib(20)` under the default
  stack-depth budget.
- `stack-overflow-throws.ts` produces a catchable diagnostic, not a
  panic.
- `stack-trace-shape.ts` matches its golden file.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter calls/
cargo bench -p otter-vm --bench calls -- --quick
```

## Risks

- **Per-call `Vec` allocation.** Easy to introduce by reusing a
  generic argument-collection helper. The zero-alloc test is the gate.
- **Stack overflow → process abort.** Without `CheckStackDepth`, deep
  recursion crashes the host. The opcode and its limit are not
  optional.
- **Span loss on errors.** Stack traces with empty spans are useless.
  The golden-file fixture catches that regression.
- **Closure scope creep.** Resist landing variable capture here. It
  has its own slice with its own design notes.

## Next task

This is the last task in the foundation batch. Once it lands, plan the
next batch covering objects, shapes, arrays, primitive-receiver lookup
generalization, builtin families, and the conformance ratchet (M8–M12
in the foundation plan).

When opening that next batch, audit:

- the staging directory's promotion readiness (ADR-0001),
- the cleanup map (task `01`) and whether deletion tasks should now
  schedule,
- the conformance baseline (`ES_CONFORMANCE.md`).

## Status

- not started
- last update: —
- artifacts: frame model, call/return opcodes, stack-depth limit,
  `tests/engine/calls/` fixtures, calls benchmark
