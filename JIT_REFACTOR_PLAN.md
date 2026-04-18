# Otter JIT Refactor Plan

> Status: the entire `M_JIT_A` → `M_JIT_C.3` track has shipped. The v2
> baseline runs on aarch64 and x86_64 with tag-guarded int32 loads,
> bailout contract, mid-loop OSR, feedback-driven trust-int32 elision,
> and loop-local register pinning. Feedback-warm `bench2.ts sum` clocks
> **1 ns/inner-iter** on aarch64. Feature-track milestones (M0 → M20
> as of writing) land on top of the completed JIT stack — see
> [`V2_MIGRATION.md`](./V2_MIGRATION.md) for feature status and the
> Implementation Log below for JIT history.
>
> Rule: this document tracks the **design direction** behind the JIT,
> not the feature roadmap. Feature milestones live in
> `V2_MIGRATION.md`; per-JIT-milestone detail lives in the
> Implementation Log.

## Current State

- **Source compiler** covers the v2 feature track per
  [`V2_MIGRATION.md`](./V2_MIGRATION.md); lives at
  [`crates/otter-vm/src/source_compiler/`](crates/otter-vm/src/source_compiler/).
- **v2 dispatcher**
  ([`crates/otter-vm/src/interpreter/dispatch.rs`](crates/otter-vm/src/interpreter/dispatch.rs))
  ships ≈70 opcodes: arithmetic, comparisons, property access (named +
  keyed), upvalues, globals, calls (`CallUndefinedReceiver`,
  `CallAnyReceiver`, `CallProperty`, `CallDirect`), `Construct`,
  `TailCall`, iteration, coercions, TDZ guards.
- **v2 baseline JIT**
  ([`crates/otter-jit/src/baseline/mod.rs`](crates/otter-jit/src/baseline/mod.rs))
  — acc-aware `TemplateInstruction` IR + host-native emitters for
  aarch64 and x86_64. Tag-guarded int32 loads, `(bailout_pc,
  bailout_reason, accumulator_raw)` on side exits, per-loop-header OSR
  trampolines, feedback-driven `trust_int32` guard elision, and
  loop-local register pinning.
- **Tier-up**: `DefaultTierUpHook` in
  [`crates/otter-jit/src/tier_up_hook.rs`](crates/otter-jit/src/tier_up_hook.rs)
  threads the persistent `FeedbackVector` into compilation, invalidates
  the cache on deopt, and routes both function-entry and mid-loop
  entries.

### Remaining refinements (non-blocking)

1. **Direct in-process stencil calls from Rust tests on Apple Silicon
   remain hostile.** Smoke coverage uses the real `otter` subprocess;
   production CLI/runtime execution is canonical.
2. **Full x86_64 release microbench sampling is expensive under
   Rosetta** — the default 50-call `bench2_microbench` exceeds the
   fixed 180-second timeout.
3. **Trust-int32 has no function-entry safety net.** A post-warmup
   call with a non-int32 parameter produces silently wrong results
   until a downstream guarded op triggers deopt demotion.
4. **Pin budget is conservative** — aarch64 uses 4 slots (`x22..x25`),
   x86_64 uses 2 (`rbp`, `r15`). The analyzer produces up to
   `MAX_PINNING_CANDIDATES = 6`.

## Benchmarks

Both benchmarks live in
[`crates/otter-jit/src/baseline/mod.rs::tests`](crates/otter-jit/src/baseline/mod.rs)
as `#[ignore]`'d release-mode tests:

```bash
cargo test -p otter-jit --release -- --ignored m1_microbench     --nocapture
cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
```

- `f(42)` interpreter (M1 shape): **496 ns/iter** on aarch64, 10⁶
  iterations.
- `bench2.ts sum(10⁶)` interpreter: **416 ns/inner-iter** on aarch64.
- `bench2.ts sum(10⁶)` JIT (feedback-warm recompile): **1
  ns/inner-iter** on aarch64. The benchmark reuses a warmed
  `RuntimeState` so `try_compile` consumes the persistent feedback
  vector and activates both M_JIT_C.2 guard elision and M_JIT_C.3
  pinning.
- `bench2.ts sum(10⁶)` x86_64 Rosetta local sample: **1348
  ns/inner-iter** interp vs **3 ns/inner-iter** JIT, measured with
  `OTTER_BENCH2_CALLS=1 OTTER_BENCH2_WARMUP_CALLS=1`.

---

## JIT Milestones

`M_JIT_A` (aarch64 tag guards + bailout) → `M_JIT_B` (x86_64 backend)
→ `M_JIT_C.1` (mid-loop OSR) → `M_JIT_C.2` (trust-int32 elision) →
`M_JIT_C.3` (loop-local register pinning) — all shipped. Commit
hashes in [`V2_MIGRATION.md`](./V2_MIGRATION.md); design + shape
detail in the Implementation Log below; per-milestone commit bodies
carry the fine-grained task-by-task narrative.

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
  the unit test). Commit: `ad3d137`.
- 2026-04-17: **M_JIT_C.3 landed** — the analyzer's
  `compute_pinning_candidates` pass counts loop-body READ + WRITE
  references per slot, filters to candidates whose READ uses are all
  `trust_int32`, ranks by frequency with stable tie-breaking, and
  writes the top-6 into `TemplateProgram::pinning_candidates`. Both
  emitters pick a prefix and bind slots to callee-saved regs
  (`x22..x25` on aarch64 via `stp`-pair pushes; `rbp`/`r15` on x86_64
  via plain `push`/`pop`). The prologue loads each pinned slot into
  its reg; all `Ldar`/`Star`/`AddAcc`/`SubAcc`/`MulAcc`/`BitOrAcc`/
  `CompareAcc`/`Mov` operations on pinned slots rewrite to register
  ops; the return path pops the saved regs; `bailout_common` spills
  pinned regs back to memory with `box_int32` before popping, so the
  interpreter resume sees coherent slot state. OSR trampolines
  mirror the main prologue's save-and-load. `bench2_microbench` now
  reuses the warmed `RuntimeState` so `try_compile` activates both
  M_JIT_C.2 and M_JIT_C.3 — the JIT row drops from 2 ns/inner-iter
  to **1 ns/inner-iter** on aarch64 (the 2× target).
  `m_jit_c_3_pinned_body_skips_pinned_slot_loads` locks in the
  pinned shape by asserting the warm stencil has ≤ 12 `LDR` insns
  (unpinned has ≥ 15). Commit: `5fe7c1e`.

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
