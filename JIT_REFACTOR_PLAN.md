# Otter JIT Refactor Plan

> Status (2026-04-17): M0–M9 of the v2 migration have shipped, and
> `M_JIT_A` has now landed on top: the aarch64 baseline runs real
> source-compiled JS through `TierUpHook::execute_cached`, uses per-load
> int32 tag guards + bailout pads, and caches recursive `CallDirect`
> bodies as deopt-friendly template stencils. The next JIT milestone is
> `M_JIT_B` (x86_64 backend parity).
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
  — acc-aware `TemplateInstruction` IR + x21-pinned aarch64 emitter.
  The shipped path now translates bytecode-visible registers through the
  frame's hidden-slot base, tag-guards every int32 load, writes
  `(bailout_pc, bailout_reason, accumulator_raw)` on side exits, and
  treats `CallDirect` bodies as cacheable deopt boundaries so recursive
  functions still get template stencils. `bench2.ts sum(10⁶)` measures
  **2 ns/inner-iter** in the release microbench.
- **Tier-up plumbing**: `DefaultTierUpHook` in
  [`crates/otter-jit/src/tier_up_hook.rs`](crates/otter-jit/src/tier_up_hook.rs)
  is installed into every `OtterRuntime`. Inner calls routed through
  `CallDirect` (as of M9) decrement the JSC hotness budget and compile
  synchronously on exhaustion; the smoke path now runs through the real
  `otter` CLI and observes JIT telemetry for the hot inner function.

### What's still latent

The remaining JIT work is now downstream of `M_JIT_A`:

1. **x86_64 backend parity is still missing.** `M_JIT_B` ports the same
   template-baseline subset, guard model, and bailout contract to the
   x86_64 assembler.
2. **Direct in-process stencil calls from Rust tests on Apple Silicon
   remain hostile.** The regression-safe smoke coverage therefore uses
   the real `otter` subprocess rather than reintroducing raw harness
   calls; production CLI/runtime execution is the canonical path.
3. **OSR + speculative int32-trust remain deferred.** Back-edge entry
   and feedback-driven guard elision are still the `M_JIT_C` work.

### Benchmarks (M2 + M7)

- **`f(42)` interpreter** (M1 shape): **496 ns/iter** on aarch64, 10⁶
  iterations via `Interpreter::execute_with_runtime`.
- **`bench2.ts sum(10⁶)` interpreter**: **416 ns/inner-iter** (≈ 416
  ms/call) on aarch64, 50 calls × 10⁶ inner iters with 100-call warmup.
- **`bench2.ts sum(10⁶)` JIT**: **2 ns/inner-iter** (≈ 2.51 ms/call)
  on aarch64 via `DefaultTierUpHook::execute_cached` after
  `try_compile(sum)`.

Both benchmarks live in
[`crates/otter-jit/src/baseline/mod.rs::tests`](crates/otter-jit/src/baseline/mod.rs)
as `#[ignore]`'d release-mode tests; invoke via:

```bash
cargo test -p otter-jit --release -- --ignored m1_microbench     --nocapture
cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
```

The aarch64 JIT rows above are the `M_JIT_A` completion marker; the next
benchmark gap is x86_64 parity (`M_JIT_B`).

### Regression status (post-M9)

| Command | Pass / Total |
| --- | --- |
| `cargo test -p otter-vm --lib` | **371 / 371** |
| `cargo test -p otter-jit --lib` | **21 / 21** (2 ignored — `m1_microbench` + `bench2_microbench`) |
| `cargo build --workspace` | green |
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
   [`crates/otter-jit/src/arch/x86_64.rs`](crates/otter-jit/src/arch/x86_64.rs):
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

### M_JIT_C — OSR + speculative int32-trust (deferred)

Deferred until M_JIT_A and M_JIT_B both ship. Work items:

1. **Mid-loop OSR.** Sparkplug-style `pc_offsets` side table so a hot
   loop can enter compiled code at a back-edge without waiting for the
   next function call.
2. **Speculative int32-trust elision.** When a PC's persistent
   `ArithmeticFeedback` has stabilized at `Int32`, skip the tag guard
   for that PC. The `analyze_template_candidate_with_feedback` plumbing
   + `trust_int32: Vec<bool>` side table is already sketched;
   activation just needs feedback wiring from the source compiler side.
3. **Loop-local register allocator** (Sparkplug-per-function). Pin the
   loop-carried int32 variables into callee-saved registers so the
   body doesn't round-trip through memory on every iteration. Scope:
   ~500 LOC in
   [`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs).
   Analysis pass + modified emitter. Opens the door to 10–20×
   additional speedup on tight int32 loops.

---

## Key files to know

- **Bytecode ISA**: [`crates/otter-vm/src/bytecode/`](crates/otter-vm/src/bytecode/)
  — `opcodes.rs`, `encoding.rs`, `decoding.rs`, `operand.rs`,
  `feedback_map.rs`.
- **Dispatcher**: [`crates/otter-vm/src/interpreter/dispatch.rs`](crates/otter-vm/src/interpreter/dispatch.rs).
- **Source compiler**: [`crates/otter-vm/src/source_compiler/`](crates/otter-vm/src/source_compiler/) —
  `mod.rs` (lowering) + `error.rs` + `tests.rs`.
- **JIT baseline**: [`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs)
  (analyzer + x21-pinned aarch64 emitter).
- **Pipeline routing**: [`crates/otter-jit/src/pipeline.rs`](crates/otter-jit/src/pipeline.rs) —
  `compile_function` / `compile_function_with_feedback`.
- **Tier-up hook**: [`crates/otter-jit/src/tier_up_hook.rs`](crates/otter-jit/src/tier_up_hook.rs)
  — the **only** production path that invokes compiled stencils
  correctly on macOS.
- **Activation fields**: [`crates/otter-vm/src/interpreter/activation.rs`](crates/otter-vm/src/interpreter/activation.rs)
  — `accumulator`, `secondary_result`.
- **aarch64 assembler**: [`crates/otter-jit/src/arch/aarch64.rs`](crates/otter-jit/src/arch/aarch64.rs).

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
  Commit: `_pending_`.

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
