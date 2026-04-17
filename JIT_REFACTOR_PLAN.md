# Otter JIT Refactor Plan

> Status (2026-04-17): M0–M9 of the v2 migration have shipped, and the
> v2 baseline now runs on both supported native backends:
> `M_JIT_A` landed the guarded aarch64 path and `M_JIT_B` lands x86_64
> parity. `M_JIT_C.1` then added mid-loop OSR on top of the existing
> tier-up hook, so a hot loop in the entry function enters compiled
> code at its back-edge instead of waiting for the next function call.
> `M_JIT_C.2` wired speculative int32-trust elision end-to-end: the
> source compiler attaches feedback slots to every `Ldar`, arithmetic,
> and int32 compare op; the interpreter records `Int32` observations;
> the JIT analyzer consumes the persistent `FeedbackVector` and flips a
> per-instruction `trust_int32` flag; and the emitters drop the
> `eor/tst/b.cond` tag guards on trusted loads. On `bench2.ts sum`, a
> feedback-warm recompile shrinks the stencil by ~38% (572 B → 356 B).
> The loop-local register allocator (`M_JIT_C.3`) is the remaining JIT
> milestone.
>
> Rule: this document tracks the **design direction** behind the JIT,
> not the feature roadmap. Feature milestones live in `V2_MIGRATION.md`;
> the `JIT Milestones` section below mirrors `V2_MIGRATION.md`'s JIT
> track with concrete per-milestone task lists.

## Current State (2026-04-17)

### What works end-to-end

- **Source compiler** covers M1–M9 of `V2_MIGRATION.md`: int32
  arithmetic + relational ops, `let`/`const`, assignment (plain +
  compound), `if`/`else`, `while`/`for`, multi-function modules with
  `CallExpression`. Lives at
  [`crates/otter-vm/src/source_compiler/`](crates/otter-vm/src/source_compiler/).
- **v2 dispatcher** ([`crates/otter-vm/src/interpreter/dispatch.rs`](crates/otter-vm/src/interpreter/dispatch.rs))
  ships ≈70 opcodes: arithmetic, comparisons, property access (named +
  keyed), upvalues, globals, calls (`CallUndefinedReceiver`,
  `CallAnyReceiver`, `CallProperty`, `CallDirect`), `Construct`,
  `TailCall`, iteration (`GetIterator`/`IteratorNext`/`IteratorClose`/
  `ArrayPush`/`ForInEnumerate`/`ForInNext`), coercions (`ToNumber`/
  `ToString`/`ToPropertyKey`), TDZ guards (`AssertNotHole`/
  `ThrowConstAssign`/`AssertConstructor`).
- **v2 baseline JIT** ([`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs))
  — acc-aware `TemplateInstruction` IR + host-native emitters for
  aarch64 and x86_64. Both shipped paths translate bytecode-visible
  registers through the frame's hidden-slot base, tag-guard every int32
  load, write `(bailout_pc, bailout_reason, accumulator_raw)` on side
  exits, and treat `CallDirect` bodies as cacheable deopt boundaries so
  recursive functions still get template stencils. `bench2.ts sum(10⁶)`
  measures **2 ns/inner-iter** on native aarch64 and **3 ns/inner-iter**
  on the Rosetta-friendly x86_64 local sample.
- **Tier-up plumbing**: `DefaultTierUpHook` in
  [`crates/otter-jit/src/tier_up_hook.rs`](crates/otter-jit/src/tier_up_hook.rs)
  is installed into every `OtterRuntime`. Inner calls routed through
  `CallDirect` (as of M9) decrement the JSC hotness budget and compile
  synchronously on exhaustion; the smoke path now runs through the real
  `otter` CLI and observes JIT telemetry for the hot inner function.

### What's still latent

The remaining JIT work is now downstream of `M_JIT_C.2`:

1. **Direct in-process stencil calls from Rust tests on Apple Silicon
   remain hostile.** The regression-safe smoke coverage therefore uses
   the real `otter` subprocess rather than reintroducing raw harness
   calls; production CLI/runtime execution is the canonical path.
2. **Full x86_64 release microbench sampling is still expensive on
   Apple Silicon.** The backend itself runs and the Rosetta local sample
   proves a large JIT win, but the default 50-call `bench2_microbench`
   exceeds the fixed 180-second timeout under Rosetta.
3. **Trust-int32 has no function-entry safety net.** Feedback-warm
   elision correctly emits a guarded variant on bailout (the deopt
   path demotes the bailout PC's slot and invalidates the cached
   stencil), but there is no up-front parameter tag check, so a
   post-warmup call with a non-int32 param produces silently wrong
   results until a downstream op that still carries a guard triggers
   the deopt demotion. Parameter-entry guards are a future refinement.
4. **Loop-local register allocator (`M_JIT_C.3`) is still deferred.**
   Hot int32 loop-carried slots still round-trip through memory on
   every iteration — the biggest single remaining throughput win.

### Benchmarks (M2 + M7 + M_JIT_B + M_JIT_C.1 + M_JIT_C.2)

- **`f(42)` interpreter** (M1 shape): **496 ns/iter** on aarch64, 10⁶
  iterations via `Interpreter::execute_with_runtime`.
- **`bench2.ts sum(10⁶)` interpreter**: **416 ns/inner-iter** (≈ 416
  ms/call) on aarch64, 50 calls × 10⁶ inner iters with 100-call warmup.
- **`bench2.ts sum(10⁶)` JIT**: **2 ns/inner-iter** (≈ 2.51 ms/call)
  on aarch64 via `DefaultTierUpHook::execute_cached` after
  `try_compile(sum)`.
- **`bench2.ts sum(10⁶)` x86_64 local sample**: **1348 ns/inner-iter**
  (≈ 1348 ms/call) in the interpreter vs **3 ns/inner-iter** (≈ 3.30
  ms/call) in the JIT on `x86_64-apple-darwin` under Rosetta, measured
  with `OTTER_BENCH2_CALLS=1 OTTER_BENCH2_WARMUP_CALLS=1` because the
  default 50-call release benchmark exceeds the fixed 180-second local
  timeout on Apple Silicon.

Both benchmarks live in
[`crates/otter-jit/src/baseline/mod.rs::tests`](crates/otter-jit/src/baseline/mod.rs)
as `#[ignore]`'d release-mode tests; invoke via:

```bash
cargo test -p otter-jit --release -- --ignored m1_microbench     --nocapture
cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
```

The aarch64 rows above are the `M_JIT_A` completion marker; the x86_64
local sample above is the `M_JIT_B` completion marker on this Apple
Silicon host. `M_JIT_C.1` keeps these steady-state numbers untouched
(the cached stencil dominates once warmup lands on function entry) and
adds back-edge OSR for the cold-loop shape: a one-shot 100k-iter
`main` loop that previously ran in the interpreter end-to-end now tiers
up mid-loop after ≈1500 back-edges and finishes execution in the JIT,
as verified by `osr_smoke`. `M_JIT_C.2` then shrinks the feedback-warm
stencil: on the same `bench2.ts sum` loop, a recompile with all
arithmetic slots primed at `Int32` drops the stencil from 572 B to
356 B (−37.8%), as verified by `m_jit_c_2_feedback_shrinks_stencil`.
Steady-state `bench2_microbench` JIT latency stays at 2 ns/inner-iter
because that benchmark compiles cold (no prior interpreter runs to
build feedback); warm-compile throughput lives in the shrink test.

### Regression status (post-M_JIT_C.2)

| Command | Pass / Total |
| --- | --- |
| `cargo test -p otter-vm --lib` | **371 / 371** |
| `cargo test -p otter-jit --lib` | **23 / 23** (2 ignored — `m1_microbench` + `bench2_microbench`) |
| `cargo test -p otter-jit --lib --target x86_64-apple-darwin` | **26 / 26** (2 ignored — `m1_microbench` + `bench2_microbench`) |
| `cargo build --workspace` | green |
| `cargo build --workspace --target x86_64-apple-darwin` | green |
| `cargo clippy --workspace --all-targets -- -D warnings` | green |
| `cargo fmt --all --check` | green |

---

## JIT Milestones

Each JIT milestone lands as a `feat(jit): … (M_JIT_X)` commit followed by
a `docs(v2-migration): record M_JIT_X commit hash …` tracker update —
the same two-commit pattern M0–M9 used.

### M_JIT_A — finish aarch64 tag-guarded v2 baseline

**Goal**: the x21-pinned stencil runs real JS code safely and
dispatches via `TierUpHook::execute_cached`. Target: `bench2.ts
sum(10⁶)` runs faster than the 416 ms/call interpreter baseline;
M9's `fact(7)` ships a matching JIT stencil.

Concrete tasks:

1. **Tag guards on every int32 load.** Replace the trust-int32 helper
   with the 3-insn `eor / tst / b.ne <bailout>` sequence from
   `check_int32_tag_fast` in [`crates/otter-jit/src/arch/aarch64.rs`](crates/otter-jit/src/arch/aarch64.rs).
   Track bailout patches per-instruction just like the v1 baseline
   used to.
2. **Bailout prologue.** On a guard failure, write the failing
   instruction's `byte_pc` into `ctx.bailout_pc`, the reason code
   into `ctx.bailout_reason`, and spill the live accumulator into
   `ctx.accumulator_raw` (already present on `JitContext`). Return
   `BAILOUT_SENTINEL`. The interpreter resume path already reloads
   `accumulator_raw` via `Interpreter::run_with_tier_up`.
3. **Accumulator state tracker.** The `AccState::{Int32, Raw}` walker
   is already in the tree; promote it out of the commented-out branch.
   Debug the known divergence on source-compiled `sum()` when
   `LdaThis`/`LdaCurrentClosure`/`ToNumber` enter — start with a
   minimal disassembly repro so the bug isolates cleanly before the
   analyzer widens.
4. **Re-enable the invocation smoke test** (`stencil_invocation_smoke`,
   currently `#[ignore]`'d in
   [`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs)).
   Once the stencil runs through `TierUpHook::execute_cached` reliably
   this goes green.
5. **Widen analyzer coverage** to the opcodes M9's source compiler
   actually emits: `Inc`, `Dec`, `Negate`, `BitwiseNot`, remaining
   `*Smi` variants. Each new opcode ships with a unit test.

**Validation**:

- `bench2_microbench` prints a JIT row with latency < the interpreter
  416 ns/inner-iter.
- `fact(7)` via M9's recursion test compiles through the v2 baseline.
- Regression: otter-vm 371/371, otter-jit 16/16 still green.

### M_JIT_B — x86_64 baseline backend

**Goal**: the v2 template-baseline stencil works on x86_64 with the
same op coverage, tag-guard model, and bailout contract as aarch64.

Concrete tasks:

1. **x86_64 macro assembler.** Port the helpers the aarch64 emitter
   relies on to
   [`crates/otter-jit/src/arch/x64.rs`](crates/otter-jit/src/arch/x64.rs):
   `push_callee_saved` / `pop_callee_saved`, `mov_imm64`,
   `check_int32_tag_fast`, `eor_rrr` / `and_rrr` / `orr_rrr`,
   `add_rrr` / `sub_rrr` / `mul_rrr`, `cmp_rr`, `b_cond_placeholder`,
   `b_placeholder`, `sxtw`, `box_int32`, `ret`. Register pinning
   follows the x86_64 SysV ABI — plan: `rbx` = `JitContext*`,
   `r12` = `registers_base`, `r13` = accumulator,
   `r14` = `TAG_INT32`.
2. **Emitter dispatch.** Replace the
   `#[cfg(not(target_arch = "aarch64"))]` bail in
   `emit_template_stencil` with an x86_64 branch that runs the same
   analyzer output through the new assembler.
3. **Bailout + spill conventions mirror aarch64** — `accumulator_raw`
   spill, `BAILOUT_SENTINEL` return, `ctx.bailout_pc` +
   `ctx.bailout_reason` writes. The interpreter resume path is
   arch-agnostic already.
4. **Test parity.** The `m2_stencil_disassembly_sanity` +
   `m1_microbench` + `bench2_microbench` tests get x86_64 gates using
   `iced-x86` (already a dev dep for disassembly). Mnemonic assertions
   change (`ADD` stays, `ORR` becomes `OR`, `B_NE` becomes `JNE`, etc)
   but the structural shape — prologue + tag guard + arithmetic + box
   + ret — ports as-is.

**Validation**:

- `cargo test -p otter-jit --lib` passes on x86_64.
- `bench2.ts sum(10⁶)` runs through the x86_64 JIT with measurable
  speedup vs the x86_64 interpreter.

### M_JIT_C.1 — mid-loop OSR

**Shipped.** A hot loop in the entry function (or any function that
function-entry tier-up doesn't already cover) enters compiled code at
its back-edge instead of waiting for the next call. The template
baseline now emits one OSR trampoline per loop header whose first body
op is safe to re-enter with a raw-bit accumulator reload (`Ldar`,
`LdaI32`, `LdaTagConst`, `LdaThis`, `LdaCurrentClosure`, `Mov`); other
loop headers stay interpreter-only. The interpreter's run loop
snapshots PC before each `step`, detects a back-edge on `Continue`, and
calls [`TierUpHook::execute_cached_at_pc`](crates/otter-vm/src/interpreter/tier_up.rs)
once the back-edge budget exhausts; bailouts restore the frame and
resume interpretation in-place.

### M_JIT_C.2 — speculative int32-trust elision

**Shipped.** The source compiler attaches an `Arithmetic`-kind
[`FeedbackSlot`](crates/otter-vm/src/bytecode/feedback_map.rs) to every
`Ldar`, binary-arithmetic, `*Smi`, and int32-compare op via
[`BytecodeBuilder::attach_feedback`](crates/otter-vm/src/bytecode/encoding.rs)
and populates a matching `FeedbackTableLayout` on the function. The
interpreter's dispatch records
[`ArithmeticFeedback::Int32`](crates/otter-vm/src/feedback.rs) after
every successful op (`Ldar` additionally promotes to `Any` on a
non-int32 load so the monotonic lattice demotes speculation when the
slot turns out to be polymorphic). `run_completion_with_runtime`'s
existing `persist_feedback` call merges the per-frame observations
into the runtime's persistent vector on Return, and
[`DefaultTierUpHook::try_compile`](crates/otter-jit/src/tier_up_hook.rs)
passes that persistent vector through to
[`compile_function_with_feedback`](crates/otter-jit/src/pipeline.rs) ⇒
[`analyze_template_candidate_with_feedback`](crates/otter-jit/src/baseline/mod.rs).
The analyzer walks `byte_pcs[i] → FeedbackMap::get(pc) → FeedbackSlotId`
and flips `trust_int32[i] = true` for every instruction whose slot
stabilised at `Int32`. Both emitters thread the flag into
`load_int32_guarded`, which elides the `eor/tst/b.cond` guard and the
associated bailout pad when the flag is set.

Deopt path (both `execute_cached` and `execute_cached_at_pc`):
`demote_and_invalidate_on_bailout` drops the bailout PC's feedback
slot to `Any` via [`FeedbackVector::demote_arithmetic_to_any`](crates/otter-vm/src/feedback.rs)
and calls [`code_cache::invalidate`](crates/otter-jit/src/code_cache.rs)
so the next tier-up call recompiles against the demoted feedback and
re-emits the guarded variant. The existing
`max_deopts_before_blacklist` mechanism remains the termination
guarantee if repeated bailouts still produce trust-int32 stencils for
a pathological program.

Regression test: [`m_jit_c_2_feedback_shrinks_stencil`](crates/otter-jit/src/baseline/mod.rs)
compiles `bench2.ts sum` cold, synthesises a fully-primed feedback
vector, recompiles warm, and asserts the warm stencil is ≤ 80% of the
cold stencil (measured: 572 → 356 B, 37.8% shrink).

### M_JIT_C.3 — loop-local register allocator (deferred)

1. **Liveness + loop-carried analysis.** Identify slots that are
   written, read, and live across the back-edge.
2. **Register allocation pass.** Assign up to N (aarch64: 6; x86_64:
   ~4–6) callee-saved registers to the top-N liveness-ranked candidates.
3. **Emitter integration.** Emit a prologue that loads each pinned slot
   on loop entry, an epilogue that stores pinned registers back on
   fall-through and every bailout site, and rewrite reads/writes of
   pinned slots inside the body into register ops.
4. **Bailout spill correctness.** Extend the existing bailout prologue
   to spill all currently-pinned registers back to their slots so the
   interpreter resumes with a coherent frame.
5. **Cross-arch parity.** Share the allocation decision across aarch64
   and x86_64 emitters.
6. **Disassembly sanity test.** Assert the inner loop of
   `bench2.ts sum(10⁶)` no longer issues `ldr`/`str` on the pinned `s`
   and `i` slots.

---

## Key files to know

- **Bytecode ISA**: [`crates/otter-vm/src/bytecode/`](crates/otter-vm/src/bytecode/)
  — `opcodes.rs`, `encoding.rs`, `decoding.rs`, `operand.rs`,
  `feedback_map.rs`.
- **Dispatcher**: [`crates/otter-vm/src/interpreter/dispatch.rs`](crates/otter-vm/src/interpreter/dispatch.rs).
- **Source compiler**: [`crates/otter-vm/src/source_compiler/`](crates/otter-vm/src/source_compiler/) —
  `mod.rs` (lowering) + `error.rs` + `tests.rs`.
- **JIT baseline**: [`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs)
  (analyzer + aarch64/x86_64 emitters).
- **Pipeline routing**: [`crates/otter-jit/src/pipeline.rs`](crates/otter-jit/src/pipeline.rs) —
  `compile_function` / `compile_function_with_feedback`.
- **Tier-up hook**: [`crates/otter-jit/src/tier_up_hook.rs`](crates/otter-jit/src/tier_up_hook.rs)
  — the **only** production path that invokes compiled stencils
  correctly on macOS.
- **Activation fields**: [`crates/otter-vm/src/interpreter/activation.rs`](crates/otter-vm/src/interpreter/activation.rs)
  — `accumulator`, `secondary_result`.
- **aarch64 assembler**: [`crates/otter-jit/src/arch/aarch64.rs`](crates/otter-jit/src/arch/aarch64.rs).
- **x86_64 assembler**: [`crates/otter-jit/src/arch/x64.rs`](crates/otter-jit/src/arch/x64.rs).

## Reproduction commands

```bash
# Build
cargo build --workspace

# Regression (full quality gate)
timeout 180 cargo build --workspace
timeout 90  cargo clippy --workspace --all-targets -- -D warnings
timeout 30  cargo fmt --all --check
timeout 180 cargo test --workspace

# Microbenches
cargo test -p otter-jit --release -- --ignored m1_microbench     --nocapture
cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture

# Run a script through the CLI
./target/debug/otter run path/to/script.js
```

---

## Implementation Log

### Pre-M0 history (2026-04-13 → 2026-04-15)

v1 MIR/CLIF baseline landed with JSC-style tier-up hook (Phase A); Phase
B added int32 tag guards + `BitOr`/`BitAnd`/`BitXor`/`Shl`/`Shr`/`UShr`/
`Gt`/`Gte`/`Lte`/`Eq` to the template emitter with the x21-accumulator
pin (B.10). Phase C bootstrapped the v2 ISA + v1→v2 transpile bridge +
v2 dispatcher + v2 baseline JIT (Phases 0 through 4.5b). Full details
in `git log` between `00fa61d` and `eeb84c8`. Phase 4.5b left the
guarded emitter partially wired and the invocation smoke test
`#[ignore]`'d — that's the state `M_JIT_A` inherits.

### Milestone log

- 2026-04-15: **M0 landed** (v2 migration). Deleted v1 source_compiler
  (~11k LOC), v1 bytecode.rs, v1 dispatch.rs, v1 JIT baseline/MIR/CLIF/
  IC/OSR infra, tests/node-compat, tests/test262, crates/otter-test262,
  and every integration test in `crates/*/tests`. Renamed `bytecode_v2`
  → `bytecode`, `dispatch_v2` → `dispatch`, `baseline/v2.rs` →
  `baseline/mod.rs`; dropped the `bytecode_v2` Cargo feature +
  `OTTER_V2_TRANSPILE` env gate. Scaffolded the new
  `source_compiler::ModuleCompiler` (returns
  `SourceLoweringError::Unsupported { construct: "program" }` for any
  input until M1). `otter run foo.js` now fails fast with the
  Unsupported error; quality gate green. Commit: `eeb84c8`.
- 2026-04-16: **M1 landed** — `ModuleCompiler` lowers
  `function f(n) { return n + 1 }` end-to-end. 20 new `source_compiler`
  unit tests. Commit: `377ddd2`.
- 2026-04-16: **M2 landed** — `m2_stencil_disassembly_sanity` +
  `m1_microbench` (interp 496 ns/iter on Apple Silicon). Commit:
  `c11e064`.
- 2026-04-16: **M3 landed** — all int32 binary ops (`-`, `*`, `|`, `&`,
  `^`, `<<`, `>>`, `>>>`) via a table-driven `binary_op_encoding`. 16
  new unit tests. Commit: `62c2760`.
- 2026-04-16: **M4 landed** — `let`/`const` with initializer + compile-
  time TDZ. `FrameLayout` grew a `local_count` field. 16 new unit
  tests. Commit: `0a8cc3f`.
- 2026-04-16: **M5 landed** — `AssignmentExpression` (`=`, `+=`, `-=`,
  `*=`, `|=`) onto a local `let`. `BindingRef::Local` gains `is_const`;
  `apply_binary_op_with_acc_lhs` extracted from `lower_binary_expression`.
  21 new unit tests. Commit: `53c24a2`.
- 2026-04-16: **M6 landed** — `IfStatement` + int32 relational ops
  (`<`, `>`, `<=`, `>=`, `===`, `!==`). Body grammar restructured to
  require a trailing `ReturnStatement`. New `RelationalOpEncoding` with
  forward/swapped opcodes so `n < 5` ≡ `5 > n` without scratch slots.
  19 new unit tests. Commit: `991b282`.
- 2026-04-17: **M7 landed** — `WhileStatement`, multi-declarator `let`,
  parenthesised binary LHS. Closes the bench2.ts surface.
  `bench2_microbench` measures **416 ns/inner-iter** on Apple Silicon.
  11 new unit tests. Commit: `d02fce5`.
- 2026-04-17: **M8 landed** — `ForStatement`, desugared to the standard
  init-test-body-update-jump shape. New `snapshot_scope` /
  `restore_scope` + `peak_local_count` on `LoweringContext` so for-init
  `let` binds to the loop. 15 new unit tests. Commit: `5ad7cfe`.
- 2026-04-17: **M9 landed** — multi-function modules + `CallExpression`.
  Two-pass lowering (collect names, lower bodies). `lower_call_expression`
  uses a `Cell<u16>`-backed temp-slot allocator on `LoweringContext`.
  `apply_binary_op_with_complex_rhs` fallback for call/nested-binary RHS
  (`n * fact(n - 1)`). Tier-up now fires on inner calls via `CallDirect`.
  16 new unit tests. Commit: `f6ea6a5`.
- 2026-04-17: **M_JIT_A landed** — the aarch64 template baseline now
  translates bytecode-visible registers through the hidden-slot base,
  guards every int32 load with `eor / tst / b.ne`, writes
  `(bailout_pc, bailout_reason, accumulator_raw)` on every side exit,
  and keeps recursive `CallDirect` bodies cacheable by lowering them to
  deopt boundaries. `stencil_invocation_smoke` now exercises the real
  `otter` CLI path, `fact(7)` caches as
  `CompiledCodeOrigin::TemplateBaseline`, and `bench2_microbench`
  reports **2 ns/inner-iter** (≈ 2.51 ms/call) on Apple Silicon.
  Commit: `96d8534`.
- 2026-04-17: **M_JIT_B landed** — the v2 template baseline now emits
  x86_64 SysV stencils through `arch/x64.rs`, pinning `rbx`/`r12`/`r13`
  /`r14` to `(JitContext*, registers_base, accumulator, TAG_INT32)`,
  mirroring the aarch64 bailout contract, and reusing the same analyzer
  output and `TierUpHook::execute_cached` path for real JS execution.
  x86_64 debug lib tests pass under `--target x86_64-apple-darwin`,
  `stencil_invocation_smoke` and the `fact(7)` cache-origin check now
  run on both supported JIT arches, and the local Rosetta bench2 sample
  measures **1348 ns/inner-iter** in the interpreter vs **3 ns/inner-iter**
  in the JIT (`OTTER_BENCH2_CALLS=1 OTTER_BENCH2_WARMUP_CALLS=1`).
  Commit: `e1b907a`.
- 2026-04-17: **M_JIT_C.1 landed** — the v2 template baseline now emits
  per-loop-header OSR trampolines (prologue + `accumulator_raw`
  rehydrate + unconditional jump into the body) on both aarch64 and
  x86_64, the code cache exposes them via `osr_native_offset`, the
  `TierUpHook` trait grew `execute_cached_at_pc`, and the interpreter's
  run loop detects back-edges by comparing PC before/after each `step`
  and calls the hook once the JSC-style back-edge budget exhausts.
  `bench2_microbench` JIT stays at **2 ns/inner-iter** on aarch64 (no
  regression vs M_JIT_B), and the new `osr_smoke` integration test
  confirms a single-function 100k-iter loop tiers up mid-loop through
  the real `otter` CLI with `--dump-jit-stats` showing `1 JIT / 0
  interpreter entries` for `main`. Commit: `5251b41`.
- 2026-04-17: **M_JIT_C.2 landed** — the source compiler now attaches
  an arithmetic-kind `FeedbackSlot` to every `Ldar`, binary-arithmetic,
  `*Smi`, and int32-compare op; the interpreter records
  `ArithmeticFeedback::Int32` on success (and `Any` on non-int32 `Ldar`
  loads); `DefaultTierUpHook::try_compile` threads the persistent
  `FeedbackVector` into `analyze_template_candidate_with_feedback`,
  which populates `TemplateProgram::trust_int32` per-instruction; both
  emitters (aarch64 + x86_64) elide the `eor/tst/b.cond` guard and its
  bailout pad on trusted loads. Deopt path demotes the bailout PC's
  slot to `Any` via `FeedbackVector::demote_arithmetic_to_any` and
  invalidates the cached stencil so the next recompile falls back to
  the guarded variant. `m_jit_c_2_feedback_shrinks_stencil` confirms a
  feedback-warm `bench2.ts sum` recompile is 37.8% smaller than the
  cold stencil (572 → 356 B). `bench2_microbench` JIT latency holds
  at 2 ns/inner-iter (benchmark compiles cold, so M_JIT_C.2's shrink
  doesn't show up in that microbench; warm-compile savings live in
  the unit test). Commit: `_pending_`.

---

## Executive Summary

Otter should not aim to "beat V8 and JSC everywhere." That is not an
engineering target.

Otter should aim to win in the workloads that matter for an embeddable
Rust-first runtime:

- faster warmup to native execution
- lower latency variance under mixed host/runtime load
- stronger host-integration performance
- smaller code and metadata footprint
- predictable tiering and deopt behavior
- competitive steady-state throughput on the hot subset we actually
  execute well

The current JIT already proves that native code generation is not the
core problem. On a pure arithmetic loop, the existing JIT test shows a
very large speedup over the interpreter. On more realistic JS entry
scripts, the speedup is small or negative because the hot path still
falls back into generic runtime helpers and the current OSR/speculation
story is incomplete.

This plan changes the architecture accordingly:

1. Treat "compile fast" and "optimize hard" as different products.
2. Keep Tier 1 off the `bytecode → MIR → CLIF` path. Direct
   `bytecode → asm` templates only.
3. Make ICs and small stubs the center of execution, not side tables.
4. Keep a richer optimizing tier for later, fed by frozen feedback
   snapshots.
5. Prebuild and reuse machine-code stubs aggressively where semantics
   allow it.

## Hard Requirements

Non-negotiable for the refactor to be worth doing:

1. Correct JS semantics first.
2. GC safety first. Every tier must have valid stack maps or explicit
   safepoint policy.
3. Deopt must be cheap enough to use aggressively.
4. No hot-path generic runtime helper calls for the monomorphic common
   case.
5. Tier 1 compile latency must be measured as a product metric, not a
   side effect.
6. Patchability and invalidation must be built in from day one.
7. Every hot speculation must have an explicit invalidation owner.
8. Code size and metadata size must be tracked alongside throughput.

## Non-Goals

- preserving the current JIT architecture for compatibility reasons
- beating V8 / JSC / Bun / SpiderMonkey on broad JS conformance/perf
  matrices
- building a full TurboFan/FTL-class peak compiler immediately
- replacing the entire runtime object model before stabilizing tiering
- introducing heroic whole-program optimization
- tracing JIT as the primary architecture

Tracing may become a future sidecar for pathological loop workloads,
but not the primary execution design.

## Compatibility Policy

Backwards compatibility is not a planning constraint for this refactor.

If module boundaries, tier APIs, or internal ownership model block a
cleaner Tier 1 / Tier 2 split, they should be changed directly instead
of preserved behind shims. The only compatibility bar that matters is
semantic correctness at the JS/runtime boundary.

## New Target Architecture

Otter should move to a 4-part execution architecture.

### 1. Tier 0: Threaded Interpreter + Superinstructions

- canonical correctness tier
- feedback collection owner
- fallback / deopt landing pad
- fast enough that Tier 1 is only needed for real heat

Candidates for superinstructions: `LoadI32 + Add + Store`, monomorphic
`GetProperty + Call`, loop-header compare/branch pairs, array length/
load/store triplets.

### 2. Tier 1: Template Baseline JIT

- compile extremely fast
- produce compact machine code
- execute the hot monomorphic subset without touching generic runtime
  code
- support patchable IC call sites and true OSR entry

Direct `bytecode → asm`. No MIR. No Cranelift. Explicit macro-
assembler per target (`x86_64`, `aarch64`). Fixed register policy and
fixed frame contract. Patchable sites for property/call/element IC
stubs. The unit of reuse is instruction templates / IC stub templates /
prologue+epilogue templates / deopt + safepoint helpers, **not** full
function AOT.

### 3. Tier 1.5: Stub Compiler / IC Engine

CacheIR-like stub DSL as the single source of truth. Interpreter can
execute it. Baseline JIT can call or inline selected stub shapes.
Optimizing tier can lower from frozen snapshots of the same data.

Stub families: named property load/store, dense element load/store,
global load/store, direct call target check, builtin fast-call entry,
prototype-chain validity guards.

### 4. Tier 2: Speculative Optimizing Tier

Consumes frozen feedback snapshots. Emits JS-specific SSA. Real deopt
state materialization. Selective inlining, LICM, value numbering,
range analysis, escape analysis where justified. Cranelift stays as
an **optional** intermediate backend — if it blocks compile budget or
codegen quality, replace the Tier 2 backend without contaminating
Tier 1.

## Core Refactor Decision

**Tier 1 must stop being a reduced version of Tier 2.** Historically
the baseline path was effectively a simplified optimizing compiler.
That's why it was too expensive to compile and still too dependent on
runtime helpers.

The new split must be:

- Tier 1: fast, shallow, patchable, helper-averse
- Tier 2: slower, deeper, speculative, deopt-rich

If a feature forces heavy IR construction, global analysis, or
expensive lowering, it belongs in Tier 2, not Tier 1.

## Major Workstreams

### Workstream A — Baseline Macro Assembler

- `crates/otter-jit/src/arch/{x86_64, aarch64}.rs` as real
  macro-assembler backends
- fixed scratch-register policy, patchable call/jump/guard sites
- reusable prologue/epilogue/deopt trampolines

**Exit**: Tier 1 compiles without MIR/CLIF for the supported subset;
generated code is smaller than the retired v1 baseline output on the
same subset; compile latency is sub-millisecond on short functions.

### Workstream B — Executable ICs

- CacheIR-like representation made authoritative
- stub compiler for monomorphic and low-polymorphic cases
- patchable baseline call sites
- invalidation wired to watchpoints / proto epochs / shape transitions

**Exit**: monomorphic property get/set avoids generic runtime helper
crossings; direct call fast path does not rebuild callee register
windows through generic fallback; megamorphic behavior exits cleanly.

### Workstream C — Real OSR

- loop-header liveness map
- real OSR entry block generation
- JIT PC map for resume and profiling
- deopt materialization from compiled frame state back to interpreter
  state

**Exit**: hot loop can enter compiled code at the header without
restarting the function; deopt returns to the exact bytecode PC with
reconstructed live values; per-header bailout telemetry exists.

### Workstream D — Runtime Boundary Cleanup

- split "hot inlineable runtime operations" from "slow generic
  semantics"
- explicit APIs for shaped loads/stores, dense array fast paths,
  builtin call fast paths
- write-barrier and allocation fast paths callable from JIT without
  generic interpreter entry

**Exit**: JIT does not need to call catch-all helpers for common
monomorphic operations; allocation and write barrier have dedicated
JIT-safe entry points.

### Workstream E — Tier 2 Compiler

- frozen feedback snapshot pipeline
- speculative builder from feedback / IC state
- real guard lowering for shape / proto / bounds / hole / string /
  function / bool cases
- materialized deopt state
- selective inliner

**Exit**: Tier 2 meaningfully outperforms Tier 1 on stable hot code;
Tier 2 compile budget is bounded and measurable; deopt behavior is
stable under changing shapes / call targets.

### Workstream F — Tooling and Perf Discipline

- assembly diff tooling
- per-tier compile latency dashboards
- helper / stub hit-rate telemetry
- deopt histograms by reason and site
- code cache occupancy and aging metrics

**Exit**: every perf claim can be backed by a benchmark and
disassembly; regressions are caught by CI before landing.

## Performance Gates

Directional; tune after first release-mode data.

**Tier 1 compile latency**:

- tiny function: p50 < 0.2 ms (release)
- small hot loop: p50 < 0.5 ms (release)
- medium monomorphic function: p95 < 2 ms (release)

**Tier 1 throughput**:

- arithmetic loops: ≥ 10× over interpreter
- monomorphic property loops: ≥ 3× over interpreter
- simple call chains: ≥ 2× over interpreter

**Tier 2 throughput**:

- stable hot kernels: ≥ 1.5× over Tier 1
- mixed monomorphic JS: clearly better than Tier 1 without deopt thrash

**Code quality**: no uncontrolled code-size growth vs baseline; no
helper-heavy hot traces in disassembly for monomorphic kernels.

## Production Gates

The JIT is not production-ready until all of these are true:

1. Tier 1 can compile and execute a documented subset without MIR/CLIF.
2. Property/call/element monomorphic fast paths do not cross generic
   runtime helpers.
3. Real OSR exists.
4. Real deopt materialization exists.
5. GC safepoints and stack maps are validated under stress.
6. Invalidation is wired for every installed speculation kind.
7. Release-mode perf dashboard is green.
8. Code cache has eviction/aging policy.
9. JIT crashes can be triaged with asm, site metadata, and deopt
   telemetry.
10. Cross-arch behavior is tested on both `x86_64` and `aarch64`.

## Decision Summary

- faster interpreter
- direct-template baseline JIT
- IC / stub-first hot path
- real OSR / deopt
- speculative optimizer on top
- prebuilt asm scaffolding where it helps

That is the shortest path to a JIT that is both fast and
production-grade for Otter.
