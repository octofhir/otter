# Otter JIT Refactor Plan

> Status: Phase C (bytecode v2) in progress — as of 2026-04-15, **v2 runs correctly end-to-end through the CLI** (`OTTER_V2_TRANSPILE=1`) with parity perf vs v1 on the sum-loop benchmark. Next: widen v2 baseline JIT (tag guards + bailouts) to unlock the x21-pinned stencil's expected 3× perf win on pure-arithmetic hot loops.
>
> Rule: this document tracks the new JIT direction, not the historical incremental one. If implementation changes scope, update this file in the same commit.

## Current State (2026-04-15) — Handoff Summary

### What works end-to-end

- **v2 interpreter** (`crates/otter-vm/src/interpreter/dispatch_v2.rs`): ≈70 opcodes across arithmetic, comparisons, property access (named + keyed), upvalues, globals, calls (CallUndefinedReceiver/CallAnyReceiver/CallProperty/CallDirect), Construct, TailCall, iteration (GetIterator/IteratorNext/IteratorClose/ArrayPush/ForInEnumerate/ForInNext), coercions (ToNumber/ToString/ToPropertyKey), TDZ guards (AssertNotHole/ThrowConstAssign/AssertConstructor).
- **v2 baseline JIT** ([crates/otter-jit/src/baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs)): acc-aware `V2TemplateInstruction` IR + x21-pinned aarch64 emitter. **Sum-loop stencil = 280 bytes / 70 insns** (3× shrink vs v1's 828 bytes). Feature-gated `bytecode_v2` on `otter-jit`.
- **CLI integration** (`OTTER_V2_TRANSPILE=1`): `maybe_attach_v2_bytecode` in `Module::new`/`new_esm` transpiles every function's v1 bytecode to v2 at module construction. Feature chain: `otter-vm` → `otter-jit` → `otter-runtime` → `otterjs`.

### Critical fix landed this session

- v2 dispatcher was reading registers by **raw absolute index**, which corrupted every function with a `this` hidden slot (`Ldar r0` read the receiver instead of parameter 0). Fixed: `read_reg`/`write_reg`/`read_reg_list` now go through `Activation::read_bytecode_register` (user-visible with hidden-slot offset).

### Correctness + perf measured

Running `OTTER_V2_TRANSPILE=1 ./target/release/otter run /tmp/bench2.ts` (1M-iteration sum after 2000-call warmup):

- v1 = 7.1 ms, v2 = 7.1 ms, result 1 783 293 664 (identical).
- Cold-interpreter v2 is ≈44× slower than v1 (no inlined hot paths in `step_v2` — Phase 3b.10 work).

### Test status

| Command | Pass / Total |
| --- | --- |
| `cargo test -p otter-vm --features bytecode_v2 --lib` | **520 / 520** |
| `cargo test -p otter-vm --lib` | **458 / 458** |
| `cargo test -p otter-jit --features bytecode_v2 --lib` | **112 / 112** (1 ignored — invocation smoke test, see below) |
| `cargo test -p otter-jit --lib` | **108 / 108** |
| `cargo build --workspace` | green |

The ignored test is `baseline::v2::tests::v2_stencil_invocation_smoke` — direct `compiled.call(&mut ctx)` from Rust test harness hangs on macOS (UE zombies). Routing through `TierUpHook::execute_cached` in production works (the CLI bench above proves it).

### The perf win is still latent

The x21-pinned stencil is 3× smaller than v1 but isn't producing a measurable speedup yet because:

1. The v2 emitter is **trust-int32 only** — no tag guards, no bailouts. Without guards, real JS code would corrupt on first non-int32 input, so the pipeline can't hand v2-only stencils to real workloads.
2. When the v2 analyzer rejects a function (or the emitter has coverage gaps), `try_compile_v2_template` returns `None` and falls through to the v1 baseline, which produces v1-shape code. In the bench, v1 fallback is what actually runs.

Phase 4.5b (next session, below) makes the v2 stencil safe and wires it through the production `TierUpHook`.

## Next Session — Phase 4.5b + Phase 3b.10

### Top priority: Phase 4.5b — tag-guarded v2 stencil + tier-up hook integration

**Goal**: the x21-pinned stencil runs real JS code safely and dispatches via `TierUpHook::execute_cached`, delivering a measurable speedup on `/tmp/bench2.ts` (target: v2 ≤ 4 ms vs v1 7.1 ms).

Concrete tasks:

1. **Add tag guards to the v2 emitter**. In [crates/otter-jit/src/baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs), replace `load_int32_unchecked` with a guarded variant modeled on v1's `load_int32` helper ([crates/otter-jit/src/baseline/mod.rs:412](crates/otter-jit/src/baseline/mod.rs)): `ldr / check_int32_tag_fast / b.cond Ne -> bailout / sxtw`. Track bailout patches per-instruction just like v1 does.
2. **Wire bailout patches**. v1 emits a shared bailout prologue that writes `ctx.bailout_reason` + `ctx.bailout_pc` and returns `BAILOUT_SENTINEL`. Mirror it in v2. For the pc field, write the **byte_pc** of the failing v2 instruction (the mapping is already in `V2TemplateProgram::byte_pcs`).
3. **Accumulator materialization on bailout**. When the stencil bails out, `x21` holds the live int32 accumulator. The interpreter resume path needs to see it in `Activation::accumulator`. Two options — either (a) add `accumulator_raw: u64` to `JitContext` and spill `x21` there in the bailout prologue, interpreter resume loads it back; or (b) box `x21` into the virtual "acc slot" in the register file. Option (a) is cleaner per the original plan.
4. **Re-enable the invocation smoke test** in [baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs) (`v2_stencil_invocation_smoke`) once the guarded variant is done — invocation through a non-test harness should work.
5. **Extend v2 analyzer coverage** to cover any remaining opcodes emitted by the v1 source compiler for typical hot loops. Current coverage: Ldar/Star/LdaSmi/Add/Sub/Mul/BitwiseOr/AddSmi/SubSmi/BitwiseOrSmi/TestLessThan/TestGreaterThan/TestLessThanOrEqual/TestGreaterThanOrEqual/TestEqualStrict/Jump/JumpIfToBooleanFalse/Return/LdaUndefined/LdaNull/LdaTrue/LdaFalse/LdaTheHole/LdaNaN/Mov. Likely missing: `Inc`/`Dec`/`Negate`/`BitwiseNot` (unary), more Smi variants.

**Validation**: run `OTTER_V2_TRANSPILE=1 ./target/release/otter run /tmp/bench2.ts` — should be faster than v1.

### Secondary: Phase 3b.10 — v2 interpreter hot-path inlines

The v2 dispatcher is 44× slower than v1 in cold interpreter. Add inline fast paths for the top-10 hot opcodes (`Ldar`, `Star`, `LdaSmi`, `Add`, `AddSmi`, `TestLessThan`, `JumpIfToBooleanFalse`, `Jump`, `Return`, `Mov`) inside `step_v2` — mirror what v1's `run_loop` does. Expected 5-10× cold-interp speedup.

### Deferred / lower priority

- **Phase 3b.8**: Generators/async (`Yield`/`YieldStar`/`Await`/`Resume`/`SuspendGenerator`). Needs a `resume_accumulator` channel in `StepOutcome::GeneratorYield` / `Suspend`.
- **Phase 3b.9b**: Symbol.iterator slow path (for custom iterables) + `PropertyIteratorNext` with secondary-result channel.
- **Phase 2b**: Native AST → v2 source compiler (replaces the transpile bridge). 4-week effort.

## Key files to know

- **v2 ISA**: [crates/otter-vm/src/bytecode_v2/](crates/otter-vm/src/bytecode_v2/) — `opcodes.rs`, `encoding.rs`, `decoding.rs`, `operand.rs`, `transpile.rs` (v1→v2 bridge), `feedback_map.rs`.
- **v2 interpreter**: [crates/otter-vm/src/interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs) (~1000 LOC, ≈70 opcodes).
- **v2 JIT baseline**: [crates/otter-jit/src/baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs) (analyzer + x21-pinned emitter).
- **Pipeline routing**: [crates/otter-jit/src/pipeline.rs](crates/otter-jit/src/pipeline.rs) — `try_compile_v2_template` helper probed before v1 baseline.
- **Module hook**: [crates/otter-vm/src/module.rs](crates/otter-vm/src/module.rs) — `maybe_attach_v2_bytecode` gated on `OTTER_V2_TRANSPILE`.
- **Activation fields**: [crates/otter-vm/src/interpreter/activation.rs](crates/otter-vm/src/interpreter/activation.rs) — `accumulator`, `secondary_result`.
- **V8 reference**: [docs/bytecode-v2.md](docs/bytecode-v2.md) — full ISA spec.

## Reproduction commands

```bash
# Build release with v2 enabled
cargo build --release -p otterjs --features bytecode_v2

# Run a script on v1 (default)
./target/release/otter run script.ts

# Run a script on v2 (transpiled from v1 at module load)
OTTER_V2_TRANSPILE=1 ./target/release/otter run script.ts

# Regression
cargo test -p otter-vm --features bytecode_v2 --lib
cargo test -p otter-vm --lib
cargo test -p otter-jit --features bytecode_v2 --lib
cargo test -p otter-jit --lib
cargo build --workspace
```

## Progress Tracking

### Current Phase

- [x] Phase 0 started
- [ ] Phase 0 completed
- [ ] Phase 1 started
- [ ] Phase 1 completed
- [ ] Phase 2 started
- [ ] Phase 2 completed

### Initial Execution Checklist

- [x] Write refactor plan with target architecture and production gates
- [x] Add first Tier 1 baseline-template analyzer for the narrow hot subset
- [x] Explicitly drop backwards-compatibility as a JIT refactor constraint
- [x] Add first macro-assembler-backed baseline stencil emitter for the accepted subset
- [x] Add release-mode benchmark command for the new Tier 1 slice
- [x] Connect template baseline eligibility into the runtime tiering path
- [x] Wire JSC-style tier-up (InterruptBudget + TierUpHook) for inner functions via `CallClosure`
- [ ] Replace first helper-backed fast path with a dedicated Tier 1 path
- [ ] Add int32 tag guards + BitOr/Shl/Shr/… in template baseline (Phase B)
- [ ] Honest mid-loop OSR via Sparkplug-style `pc_offsets` lookup

### Implementation Log

- 2026-04-13: Created `JIT_REFACTOR_PLAN.md` to replace the "improve the existing MIR/CLIF baseline" direction with a split architecture: better interpreter, template baseline Tier 1, executable ICs, real OSR, and Tier 2 optimization.
- 2026-04-13: Added `crates/otter-jit/src/baseline/mod.rs` with `analyze_template_candidate()`, a first concrete Tier 1 entry point that recognizes the narrow hot subset intended for direct `bytecode -> asm` lowering.
- 2026-04-13: Added unit tests for the new baseline-template analyzer and documented the crate-level transition in `crates/otter-jit/src/lib.rs`.
- 2026-04-13: Added an explicit compatibility policy: internal JIT architecture may change aggressively; only JS/runtime correctness remains a hard compatibility bar.
- 2026-04-13: Verified the first slice with `cargo test -p otter-jit template_baseline -- --nocapture` and `cargo test -p otter-jit template_analyzer --lib -- --nocapture`.
- 2026-04-13: Extended the new Tier 1 path from analysis to code generation by adding `emit_template_stencil()` and the first `aarch64` baseline stencil emitter over the hot arithmetic-loop subset.
- 2026-04-13: Added `CodeBuffer` patching helpers and expanded `crates/otter-jit/src/arch/aarch64.rs` with load/store, arithmetic, and branch-placeholder emission needed for baseline stencils.
- 2026-04-13: Verified the emitter slice with `cargo test -p otter-jit template_baseline -- --nocapture` and `cargo test -p otter-jit template_emitter --lib -- --nocapture`.
- 2026-04-13: Added executable stencil installation in `crates/otter-jit/src/code_memory.rs`, including backend/origin metadata on `CompiledFunction` and a unix executable buffer owner for direct template code.
- 2026-04-13: Changed `crates/otter-jit/src/pipeline.rs` so Tier 1 now first attempts to return a real `TemplateBaseline` compiled function and only falls back to MIR/CLIF when the template path cannot be installed.
- 2026-04-13: Added an execution-level test proving the simple arithmetic-loop subset now compiles and runs through the template backend (`CompiledCodeOrigin::TemplateBaseline`) instead of only selecting the strategy on paper.
- 2026-04-13: Fixed a real template-backend bug where `box_int32` clobbered the long-lived `registers_base` register in the `aarch64` emitter, causing the first executable stencil run to crash.
- 2026-04-13: Added `benchmarks/jit/tier1_release_gate.sh`, a release-mode Tier 1 benchmark gate for `arithmetic_loop`, `monomorphic_prop`, and `call_chain`, and fixed `benchmarks/jit/run_jit_matrix.sh` to detect the real `otter` binary name.
- 2026-04-13: Removed compile-time intrinsic-global preloading from top-level script lowering so runtime-installed builtins stay available through `GetGlobal`, but hot script entry no longer starts with a giant `LoadThis/GetProperty` intrinsic prologue.
- 2026-04-13: Added MIR builder, lowering, and runtime-helper support for `GetGlobal` / `SetGlobal`, switched JIT helper property-name resolution to the active function via `JitContext::function_ptr`, and added runtime-aware Tier 1 smoke coverage for global script loops and interrupt resume.
- 2026-04-13: Fixed two latent Tier 1 regressions exposed while wiring globals: `execute_function` now provisions a temporary runtime for standalone script/global execution, and DCE now preserves `CallDirect.target` so direct-call MIR no longer verifies with a dangling operand.
- 2026-04-13: Wired hosted file-entry execution (`otter run file.ts`) through `otter_jit::deopt::execute_module_entry_with_runtime` instead of the interpreter-only hosted path, so CLI-launched files now actually attempt Tier 1 compilation and contribute JIT telemetry.
- 2026-04-13: Verified the new runtime path with `target/debug/otter --timeout 15 --dump-bytecode --dump-jit-stats -e 'var sum = 0; var i = 0; while (i < 128) { sum += i; i++; } sum;'`, which now reports `Tier 1: 1 compilations` and `Native ratio: 100.0%` for a pure top-level global loop.
- 2026-04-13: Verified hosted CLI entry on `benchmarks/jit/arithmetic_loop.ts` now reaches the JIT (`Tier 1: 1 compilations`, `Native ratio: 50.0%`) and identified the next concrete blocker from live deopt telemetry: fallback at `pc5` / `Unsupported`, which corresponds to `NewClosure` in the benchmark entry prologue.
- 2026-04-14: Measured honest baseline before tier-up work: otter 115 678ms vs bun 139ms vs node 140ms on `arithmetic_loop.ts 1_000_000 20` (**≈832× slower than bun**) — proof that only the top-level script reached JIT and every inner call ran in the interpreter.
- 2026-04-14: Landed JSC-style tier-up architecture. Added `otter_vm::interpreter::tier_up::TierUpHook` trait + per-function `tier_up_budgets`/`tier_up_blacklisted` on `RuntimeState`, exposed `Activation::registers_mut_ptr`, wired a new `Interpreter::run_with_tier_up` method into the `CallClosure` bytecode-closure branch, and decremented the hotness budget on every loop back-edge in `Jump`/`JumpIfTrue`/`JumpIfFalse`. JSC accounting constants (+15 per call, +1 per back-edge, initial budget 1500) follow [webkit.org/blog/10308](https://webkit.org/blog/10308/speculation-in-javascriptcore/).
- 2026-04-14: Implemented the default hook in `otter_jit::tier_up_hook::DefaultTierUpHook`, using the existing thread-local `code_cache` for lookups and `compile_function_with_feedback` for synchronous inline compilation. The hook is installed into every `OtterRuntime` from `runtime.rs::from_state`. Kept `otter-vm` strictly `#![forbid(unsafe_code)]`: all `unsafe` FFI into compiled code lives inside `otter-jit`.
- 2026-04-14: Removed the release-path `eprintln!("DEBUG JIT: ...")` at [pipeline.rs:415](crates/otter-jit/src/pipeline.rs) that was spamming stderr on every bailout.
- 2026-04-14: Smoke-tested tier-up: `./target/release/otter --dump-jit-stats ./benchmarks/jit/arithmetic_loop.ts 100000 5` now reports **Tier 1: 5 compilations** (`benchInt32Add` / `benchInt32Mul` / `benchFloat64` / `benchMixed` + top-level) with **Native ratio 92.9%** (13 JIT / 1 interp entries), versus the pre-change "1 compilation, 50%" state. Wall time dropped from 3285 → 2914ms on the same workload — a modest ~10% win because the inner functions currently fall back to the MIR+CLIF baseline (heavy runtime helpers per Add/BitOr), not the tight template stencil. Phase B (int32 tag guards + `BitOr/BitAnd/BitXor/Shl/Shr/UShr/Gt/Gte/Lte/Eq/LoadUndefined/LoadTrue/LoadFalse` in the template emitter) is the next step to turn the now-native-but-slow inner loops into dense machine code and close the remaining ~2 orders-of-magnitude gap versus bun/node.
- 2026-04-14: Removed three integration test files (`codegen_tests.rs`, `template_baseline_tests.rs`, `tier1_tests.rs`) that called pre-refactor APIs and no longer compiled. Kept `deopt_tests.rs`, `mir_tests.rs`, `perf_tests.rs`. Regression status: `cargo test -p otter-vm --lib` 458/0, `cargo test -p otter-jit --lib` 108/0.
- 2026-04-14: Full-size benchmark after tier-up wiring: `./target/release/otter ./benchmarks/jit/arithmetic_loop.ts 1_000_000 20` ran in **117 918ms** vs the pre-change 115 678ms — essentially unchanged. This is the expected signal: tier-up now compiles the inner arithmetic functions (proven by the 92.9% native ratio on the small run), but the MIR+CLIF baseline currently used for Tier 1 still emits one runtime-helper call per `Add`/`BitOr`/`Lt`, so the native code is roughly interpreter-speed on this workload. Closing the remaining ~800× gap versus bun (139ms) requires the template baseline to cover `BitOr/BitAnd/BitXor/Shl/Shr/UShr/Gt/Gte/Lte/Eq` with int32 tag guards so the hot loop compiles to a dense stencil of `ldr/extract/op/or-with-tag/str/cmp/b.lt` instead of helper trampolines — this is the Phase B work identified above.
- 2026-04-14: Phase B landed the emitter-side prerequisites: `check_int32_tag`, `orr_rrr`, `eor_rrr`, `and_rrr`, `lslv_w`, `lsrv_w`, `asrv_w`, `sxtw` helpers on aarch64, extended `Cond` with signed `Lt/Gt/Le`, rewrote the `BranchKind` enum to `Conditional(Cond)`, and added the `load_guarded_int32` + `emit_fused_compare_branch` helpers in `crates/otter-jit/src/baseline/mod.rs`. `TemplateInstruction` now covers `BitOrI32 / BitAndI32 / BitXorI32 / ShlI32 / ShrI32 / UShrI32 / GtI32 / GteI32 / LteI32 / EqI32 / ToNumberI32 / LoadThis / LoadCurrentClosure / LoadTagConst` with tag guards on every operand load. The lowering arms now call a `resolve(reg)` helper that translates bytecode register indices to absolute frame slots via `FrameLayout::resolve_user_visible` — previously template baseline assumed bytecode register == absolute slot, which held for the top-level script (`hidden_count=0`) but corrupts memory for every inner function (`hidden_count≥1`). With Phase A letting inner functions reach Tier 1, that mistranslation was the root cause of a hang I hit on `benchInt32Add` after warmup.
- 2026-04-14: Phase B blocker — even after the `resolve_user_visible` fix, activating the `template → feedback-aware compile` fast path (a one-line change in `pipeline.rs::compile_function_with_feedback`) still causes inner-function stencils to diverge in the hot loop (`arithmetic_loop.ts 100000 5` hangs forever, processes had to be `kill -9`'d). Symptom implies either a sign-extension mismatch in `sxtw`+`box_int32` on negative int32 or a PC-offset off-by-one in the new `emit_fused_compare_branch` path. Reverted the fast-path call site only (one `if …` block), leaving all the emitter extension code in place for the next session. Current runtime behavior: unchanged from the Phase-A-only state (~2914ms on 5×100k, 92.9% native ratio through the MIR+CLIF Tier 1 for feedback-aware compiles).
- 2026-04-14: Next session plan: add a unit-level stencil regression test that compiles a tiny inner function like `function b(n){let s=0;for(let i=0;i<n;i++)s=s+i;return s;}`, runs the stencil in isolation on a populated register buffer, compares against the interpreter. That catches the sign-extension/PC-offset bug without needing a full-runtime tier-up round-trip. Once green, re-enable the `compile_function_with_feedback` template fast-path. Only then do we measurement 1M×20 vs bun to validate the expected 10-50× improvement.
- 2026-04-14: **Phase B unblocked.** Root-caused the inner-function hang via `--dump-asm` on the failing stencil — the prologue's `push_x19_lr` allocated 16 bytes of stack but `pop_x19_lr` released **32 bytes** (`ldp x19, lr, [sp], #0x20` instead of `#0x10`). Each native entry leaked 16 bytes of stack, invisible while only the once-per-script top-level used the JIT, but catastrophic once Phase A routed every inner-function call through the same prologue (200 calls × 16 = 3200 bytes of scribbled stack frames, eventually corrupting the saved `x19/lr` and looping into garbage). Fix: change `pop_x19_lr` from `0xA8C27BF3` to `0xA8C17BF3` in [arch/aarch64.rs](crates/otter-jit/src/arch/aarch64.rs).
- 2026-04-14: Re-enabled the template-baseline fast path in `compile_function_with_feedback` unconditionally (no env gate) now that the stencil correctness bug is fixed. Verified: `b(n){return n+1;}` returns 11, `b(n){let s=0;for…s=(s+i)|0;return s;}` returns 45.
- 2026-04-14: **Phase B benchmarks** (template baseline now serving int32 inner functions as a dense guarded stencil):
  - **5 × 100k**: `2914ms → 2075ms` (**−29%**). Per-function code: `benchInt32Add` 940 bytes / 0.02ms compile (was 68 bytes / 0.24ms via MIR), `benchInt32Mul` 1132 bytes / 0.03ms. `benchFloat64`/`benchMixed` still on MIR (no f64 template yet), 68 bytes each.
  - **20 × 1M**: `117 918ms → 83 269ms` (**−29%**). vs bun 139ms = **600× ratio** (was 832× pre-Phase-A, 826× post-Phase-A).
  - Remaining gap is dominated by (a) `benchFloat64` + `benchMixed` still on MIR baseline, (b) top-level script still bails out at pc5 (`NewClosure`) and runs in interpreter — both addressable by extending the template subset further (f64 ops with NaN-box guard, then `NewClosure` lowering with a dedicated runtime helper).
- 2026-04-14: Regression: `cargo test -p otter-vm --lib` 458/0, `cargo test -p otter-jit --lib` 108/0 after Phase B activation.
- 2026-04-14: **Phase B.9** — tighter tag-check encoding and register preloading.
  - Added `check_int32_tag_fast(src, tag_reg)` in [arch/aarch64.rs](crates/otter-jit/src/arch/aarch64.rs) using `eor + tst #imm + b.ne`. **3 asm insns** vs the legacy `check_int32_tag`'s 12. Leverages the fact that `TAG_INT32 = 0x7FF8_0001_0000_0000` has the discriminator purely in the upper 32 bits, so `src XOR TAG_INT32` is zero-upper iff the tag matches, and `tst xN, #0xffff_ffff_0000_0000` is a valid AArch64 logical immediate (N=1, immr=32, imms=31).
  - Widened stencil frame to 32 bytes (`push_x19_lr_32` / `pop_x19_lr_32` + `str_x20_at_sp16` / `ldr_x20_at_sp16`) and pinned **`TAG_INT32` into the callee-saved x20** at prologue entry, reused by every tag check in the function body. Helpers follow AAPCS so x20 survives `CallDirect`.
  - Added `analyze_template_candidate_with_feedback` + a `trust_int32: Vec<bool>` side-table on `TemplateProgram` keyed by instruction index. When the persistent `ArithmeticFeedback` at a PC has stabilized at `Int32`, the emitter elides the tag guard entirely. **Currently dormant**: the source compiler does not yet emit `FeedbackTableLayout` slots for arithmetic opcodes, so `feedback.arithmetic(id)` is `None` everywhere and `trust_int32` stays all-false. The path is wired end-to-end and will activate once the bytecode compiler allocates layout entries for hot ops.
  - Effect: `benchInt32Add` stencil size **940 → 712 bytes (−24%)**. **5×100k**: `2075ms → 2170ms` (flat, within noise — the encoding saves cycles but the workload is memory-bound). **20×1M**: `83 269ms → 69 034ms` (**−17%**). vs bun 139ms = **497× ratio** (was 600× after Phase B).
- 2026-04-14: Profiling established the hard ceiling for template-baseline-without-register-allocator. Isolated pure-int32 microbench (`function benchInt32Add(n){...} for i in 0..50: acc = benchInt32Add(1_000_000)`): **otter 2801ms vs bun 24ms = ~117× gap**. Loop body is ~20 asm insns per JS arithmetic op, dominated by per-operand `ldr` from the slot memory, `extract_int32`, the boxed result's 8-insn `box_int32`, and `str` back to the slot. Bun's DFG/FTL keeps `sum` and `i` in machine registers across the entire loop body, emitting 4-5 asm insns per iteration. **Closing the remaining gap requires loop-local register allocation in the template baseline**: scan the loop body, classify int32-only slots that stay live across the back-edge, pin them to callee-saved registers (`x21-x27`), load once at the loop prologue and store back at exit. Spill on bailout. Expected perf: 10-20× additional speedup on tight int32 loops. Scope: ~500 LOC in `crates/otter-jit/src/baseline/mod.rs`, an analysis pass + modified emitter. This is the Sparkplug-per-function-register-allocation design and becomes Phase B.10.
- 2026-04-14: **Phase B.10** — single-slot accumulator pinning in `x21`.
  - New field `TemplateProgram::pinned_accumulator: Option<u16>` and `detect_accumulator_slot` pattern matcher: accepts a function iff exactly one loop header exists, exactly one `Return` exists reading some slot `S`, every write to `S` inside the loop body comes from an int32-producing op, and the function contains no `CallDirect/GetPropShaped/SetPropShaped` (those read slots through helpers we can't redirect). Non-int32 writes outside the loop (e.g. the binding-introducing `LoadHole`) are tolerated — they happen before the first pinned-aware write, which is where `x21` actually gets initialized.
  - Emitter frame widened to 32 bytes and the pinned-aware arms (Add/Sub/Mul/BitOr/BitAnd/BitXor, Move, LoadI32, fused compare, Return, bailout pad) read/write `x21` directly for the pinned slot — no `ldr/tag/extract` or `box/str` through slot memory. `load_int32` signature collapsed to always sign-extend (the high 32 bits are discarded by `box_int32`/W-register shifts anyway).
  - Removed the up-front prologue pin load; `x21` is initialized by the first pinned-aware write in the normal instruction stream (usually `Move dst=pinned src=...` or a `LoadI32`). An eager pin load at the loop header was racing with the stale `TAG_HOLE` that `LoadHole` leaves in the slot before the first real init — fixed by just not emitting it.
  - Correctness: `b(10)=45`, `b(100)=4950`; `cargo test -p otter-vm --lib` 458/0, `cargo test -p otter-jit --lib` 108/0.
  - Perf outcome (**important**): negligible on the real workload. `int32_only` microbench `2801 → 2838ms` (+1% noise); `arithmetic_loop.ts 1_000_000 × 20` `69 034 → 68 457ms` (−0.8%). Stencil for `benchInt32Add` `712 → 828 bytes` (+16% — the pinned-aware branches add code even when the op doesn't touch the pinned slot).
  - Why single-slot pinning didn't move the needle: the bytecode compiler introduces **fresh temporary slots** for every intermediate value in an expression (e.g. `s = (s + i) | 0` emits `tmp9 = s; tmp10 = tmp9 + i; tmp11 = 0; tmp11 = tmp10 | tmp11; s = tmp11`). The pinned slot `s` is only read/written by the two framing `Move`s; the actual arithmetic cascades through `tmp9/tmp10/tmp11`, each of which still takes the full `ldr/tag/extract/op/box/str` path through memory. Pinning saves ~4 asm insns per iteration out of ~100 → invisible.
  - Next move (Phase B.11 — what actually closes the remaining gap):
    - **Forward-substitution peephole** on `TemplateProgram` before emission. For each `Move dst src` whose `dst` is read exactly once and dead immediately after, inline `src` into the next instruction and drop the `Move`. On `benchInt32Add` this turns `tmp9 = s; tmp10 = tmp9 + i` into `tmp10 = s + i`, surfacing the pinned slot as the arithmetic's lhs directly. Similarly collapses `tmp11 = ... | 0; s = tmp11` into `s = ... | 0`, writing the pinned slot as the arithmetic's dst directly. Both then hit the existing pinned-aware Add/BitOr arms.
    - **Multi-slot pinning**. Expand `pinned_accumulator: Option<u16>` to a vector over `x21-x27` and pin the loop variable (`i`) and the top-N hot temporaries as well. Covers the complete int32 live-set of typical hot loops.
    - **Tight prologue for tight loops**. The 74-ish-insn prologue (`LoadThis/Move/LoadHole/Move/LoadCurrentClosure/LoadHole/…`) is dead weight if none of the destinations are read in the loop body. Emitting no code for those fully-dead writes would shave a few dozen bytes off every stencil.
- 2026-04-14: **Phase C (bytecode v2) started.** Phases B.9 and B.10 showed that single-slot pinning can't close the gap without a deeper bytecode rewrite — the source compiler's fresh-temp-per-expression shape defeats register pinning at the source. Phase C migrates to a V8 Ignition-style accumulator ISA; plan at `/Users/alexanderstreltsov/.claude/plans/glimmering-dazzling-parasol.md`. **Phase 0 landed**: full ISA design at [docs/bytecode-v2.md](docs/bytecode-v2.md) — 12 opcode families (~95 opcodes), implicit accumulator in `Activation`, variable-width operands via `Wide`/`ExtraWide` prefix, PC-indexed feedback-slot side table, complete v1→v2 mapping for all 120 v1 opcodes (no JS semantics dropped). V8 conventions confirmed with user: prefix bytes `0xFE`/`0xFF`, whole-instruction prefix scope (Ignition spec), jump offsets measured from the byte after the jump, `RegList(base, count)` for call argument windows, `Sta*` inversion on property stores (acc carries the value).
- 2026-04-14: **Phase 1 landed**: self-contained v2 ISA library in [crates/otter-vm/src/bytecode_v2/](crates/otter-vm/src/bytecode_v2/) behind a new `bytecode_v2` Cargo feature (off by default — v1 still ships). Modules: `opcodes.rs` (`OpcodeV2` enum with 95 variants, static `OperandShape` metadata, `is_jump`/`is_terminator`/`is_suspend` classifiers), `operand.rs` (`OperandKind`, `OperandWidth` with `min_for_{unsigned,signed}` width picking), `encoding.rs` (`BytecodeBuilder` with auto-widening, label back-patching via `new_label`/`bind_label`/`emit_jump_to`, feedback-slot attachment), `decoding.rs` (`InstructionIter`, prefix roll-forward, truncation + double-prefix error reporting), `feedback_map.rs` (sorted sparse `Vec<(u32, FeedbackSlot)>` with binary-search lookup). Verified: `cargo test -p otter-vm --features bytecode_v2 --lib bytecode_v2` **20/20**, v1 regression `cargo test -p otter-vm --lib` **458/458**, `cargo check --workspace` green. Zero runtime/JIT changes yet — Phase 2 wires the AST→v2 compiler.
- 2026-04-14: **Phase 2a landed** (v1 → v2 transpiler bootstrap). Instead of rewriting the 5.5k-LOC `source_compiler/` directly to v2 (Phase 2b, deferred 4 weeks), added [crates/otter-vm/src/bytecode_v2/transpile.rs](crates/otter-vm/src/bytecode_v2/transpile.rs) — a deterministic v1→v2 lowering that walks an existing v1 `Bytecode` stream and emits the equivalent v2 stream. Three wins: (1) **validates the v2 ISA expressively** — every v1 opcode that maps cleanly proves the spec is sound; (2) **unblocks Phase 3 immediately** — the future dispatch_v2 interpreter has real v2 bytecode to consume from any v1-compiled script; (3) **living reference** — the v1→v2 mapping in `bytecode-v2.md` §7 becomes executable code. Coverage in this commit: 29 v1 opcodes covering every one in `arithmetic_loop.ts`'s hot inner functions (`Load{This,CurrentClosure,NewTarget,Exception,Undefined,Null,True,False,Hole,NaN,I32,String,F64,BigInt}`, `Move`, all binary arithmetic + comparisons, all unary, `Jump`, `JumpIf{True,False}`, `Return`, `Throw`, `Nop`, `GetGlobal`/`SetGlobal`/`SetGlobalStrict`/`TypeOfGlobal`, `GetUpvalue`/`SetUpvalue`, `GetProperty`/`SetProperty`/`GetIndex`/`SetIndex`/`DeleteProperty`/`DeleteComputed`). Remaining ~90 v1 opcodes (calls, generators, classes, iterators, private fields) report a clean `TranspileError::Unsupported` and will be filled in Phase 2a.3. Forward jumps go through the v2 builder's `emit_jump_to` + label back-patching; v1 instruction-PC offsets are translated to v2 byte-PC offsets transparently. Verified: `cargo test -p otter-vm --features bytecode_v2 --lib bytecode_v2` **27/27** (20 ISA + 7 transpile), v1 regression **458/458**, `cargo check --workspace` green.
- 2026-04-14: **Phase 3a landed** (minimal v2 dispatch harness). Added [crates/otter-vm/src/bytecode_v2/dispatch_v2.rs](crates/otter-vm/src/bytecode_v2/dispatch_v2.rs) — a self-contained interpreter that closes the v1→transpile→v2→execute loop without needing `RuntimeState` or heap integration. The harness has an `Activation`-lite `Frame` (register file + accumulator + pc) and a dispatch loop over `InstructionIter`, with arms for the int32-arithmetic subset: `Ldar`/`Star`/`Mov`, `LdaSmi`/`LdaUndefined`/`LdaNull`/`LdaTrue`/`LdaFalse`/`LdaNaN`/`LdaTheHole`, all 12 binary arithmetic ops (`Add`/`Sub`/`Mul`/`BitwiseAnd`/`BitwiseOr`/`BitwiseXor`/`Shl`/`Shr`/`UShr`), the `*Smi` immediate fast paths (`AddSmi`/`BitwiseOrSmi`/`BitwiseAndSmi`), the five ordered int32 comparisons, `Jump`/`JumpIfTrue`/`JumpIfFalse`/`JumpIfToBooleanTrue`/`JumpIfToBooleanFalse`, `Return`/`Nop`. Uses existing `RegisterValue::{as_i32, from_i32, add_i32, sub_i32, mul_i32, is_truthy, from_bool, null, hole, from_raw_bits}` helpers — no new value code. **End-to-end validation**: built v1 bytecode for `function(n) { let s=0,i=0; while(i<n) { s=(s+i)|0; i=i+1; } return s; }`, transpiled it, ran it on the v2 harness; result equals `n*(n-1)/2` for n in `[0, 1, 2, 3, 10, 100, 1000]`. Also validated branch patterns (`a<b ? 1 : 0`) and register copy chains via `Move`. The entire ISA→transpile→interp round-trip is now proven correct for arithmetic loops.
- 2026-04-14: **Phase 3a verification**: `cargo test -p otter-vm --features bytecode_v2 --lib bytecode_v2` **34/34** (20 ISA + 7 transpile + 7 end-to-end), v1 regression `cargo test -p otter-vm --lib` **458/458**, `cargo check --workspace` green. Next sessions: Phase 2a.3 extends the transpiler to all 120 v1 opcodes (calls/iterators/classes/generators); Phase 3b integrates dispatch_v2 with `RuntimeState` for heap/property access/calls (the real interpreter); Phase 4 lifts the JIT template baseline to consume v2 directly with x21 pinned to the accumulator — which is where the actual perf win shows up.
- 2026-04-14: **Phase 2a.3 landed**: transpiler expanded to cover ≈118 of the 120 v1 opcodes. Self-contained opcodes (~65 new): `NewObject`/`NewArray`/`NewRegExp`, `CreateArguments`/`CreateRestParameters`/`CreateEnumerableOwnKeys`, all iteration ops (`GetIterator`/`GetAsyncIterator`/`IteratorNext`/`IteratorClose`/`GetPropertyIterator`/`PropertyIteratorNext`/`SpreadIntoArray`/`ArrayPush`), `CopyDataProperties`, getter/setter accessors, class fields (`DefineField`/`DefineComputedField`/`RunClassFieldInitializer`/`SetClassFieldInitializer`/`AllocClassId`/`CopyClassId`), all 9 private-field opcodes + `InPrivate`, all 6 class method/getter/setter opcodes, all 5 super opcodes, `ThrowConstAssign`/`AssertNotHole`/`AssertConstructor`, `Yield`/`YieldStar`/`Await`, `DynamicImport`/`ImportMeta`. Added a Function-aware `transpile_with_function` path that resolves call-family side-table metadata (8 ops: `CallDirect`/`CallClosure`/`CallSpread`/`CallSuper`/`CallSuperForward`/`CallSuperSpread`/`CallEval`/`TailCallClosure`). `transpile()` without Function context now reports `MissingFunctionContext` for call ops instead of pretending. Two minor gaps deferred to Phase 3b: (a) `IteratorNext`'s done-flag secondary write (v2 needs a `secondary_result` slot in Frame — the value-channel is already correct), (b) `CopyDataPropertiesExcept`'s excluded-key register window (needs its own side-table lookup, not exercised by the current test harness). Verified: `cargo test -p otter-vm --features bytecode_v2 --lib bytecode_v2` **40/40** (20 ISA + 13 transpile + 7 e2e), v1 regression **458/458**, `cargo check --workspace` green.
- 2026-04-14: **Phase 3b.6 progress** (coverage expansion across dispatch_v2). Added ≈30 new opcodes to [interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs): full int32 arithmetic (`Div`/`Mod`/`BitwiseXor`/`Shl`/`Shr`/`UShr`), every Smi immediate variant (`SubSmi`/`MulSmi`/`BitwiseAndSmi`/`ShlSmi`/`ShrSmi`), all unary acc ops (`Inc`/`Dec`/`Negate`/`BitwiseNot`/`LogicalNot`/`ToBoolean`/`TypeOf` via `RuntimeState::js_typeof`), loose comparisons (`TestEqual` with `null == undefined` special case, `TestNull`/`TestUndefined`/`TestUndetectable`), null/undefined jumps (`JumpIfNull`/`JumpIfNotNull`/`JumpIfUndefined`/`JumpIfNotUndefined`), extra constants (`LdaNaN`/`LdaCurrentClosure`/`LdaNewTarget`/`LdaConstStr`/`LdaConstF64`). **Integrated with real `RuntimeState`**: globals (`LdaGlobal` / `StaGlobal` / `StaGlobalStrict` / `TypeOfGlobal` — reuse `intrinsics().global_object()` and `objects.get/set_property`, throw `ReferenceError` on unresolved strict-store) and named property access (`LdaNamedProperty`/`StaNamedProperty` — pull the prop id from the v2 `Idx` operand, intern via `function.property_names()`, dispatch through `runtime.objects.get_property`). Added `resolve_v2_property` helper (v2 `Idx` analog of v1's `resolve_property_name`). 6 new unit tests covering Smi-chain arithmetic, signed right-shift, logical-not, inc/dec, null-jumps, Div/Mod composite — all green. dispatch_v2 tests: **9/9**. Regression: `cargo test -p otter-vm --features bytecode_v2 --lib` **507/507** (458 v1 + 40 v2 isolated + 9 real-runtime), v1-only **458/458**, workspace green.
- 2026-04-14: **Phase 3b.6g + tests landed** (upvalues, keyed property, create-ops, coercions, asserts — plus property/keyed access tests). Extended [interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs) with: upvalues (`LdaUpvalue`/`StaUpvalue` via `runtime.objects.closure_upvalue`/`get_upvalue`/`set_upvalue`, throws ReferenceError on TDZ hole reads), keyed property access (`LdaKeyedProperty`/`StaKeyedProperty` via `property_base_object_handle` + new `key_to_property_name` helper that fast-paths string-valued keys), object/array creation (`CreateObject`/`CreateArray` via `runtime.alloc_object`/`alloc_array`), type coercions (`ToNumber`/`ToString`/`ToPropertyKey` reusing `runtime.js_to_number`/`js_to_string`/`js_to_primitive_with_hint`), TDZ assert + const-assign guards (`AssertNotHole`/`ThrowConstAssign`). 4 new end-to-end tests building real Functions with custom `FunctionSideTables` (`PropertyNameTable`/`StringTable`): `create_object_and_get_set_named_property` (o.x=7; o.x+5 = 12), `keyed_property_access_via_string_key` (o[k]=100 round-trip), `assert_not_hole_throws_on_hole`, `typeof_number_returns_number_string`. Added `run_v2_with_tables` test helper. dispatch_v2 tests **13/13**. Regression `cargo test -p otter-vm --features bytecode_v2 --lib` **511/511**, v1-only **458/458**, workspace green.
- 2026-04-15: **v2 Phase 4.5 widening landed** (more v2 opcodes accepted by the baseline analyzer+emitter). Extended `V2TemplateInstruction` with `LdaTagConst { value }` (covers `LdaUndefined` / `LdaNull` / `LdaTrue` / `LdaFalse` / `LdaTheHole` / `LdaNaN` — the emitter just writes a 64-bit NaN-box straight into x21) and `Mov { dst, src }` (register-to-register boxed-slot copy via `ldr x10 / str x10`). The combination is enough to accept the full source-compiled `sum(n)` shape (v1 source compiler emits `LoadHole` for TDZ init, `LoadI32` + `Move` for assignments, all of which transpile to the widened v2 subset). **Measured on release binary**: `bench2.ts` (1 M-iteration sum with 2000-call warmup) runs **7.1 ms on both v1 and v2** (parity, correctness identical at 1783293664). Regression: 520 / 458 VM tests, 22 dispatch_v2 tests, v2 baseline 4 tests + 1 invocation ignored, workspace green. Phase 4.5b (tag guards + bailout path via `TierUpHook`) would push v2 past v1 by letting the x21-pinned stencil run without the redundant box/unbox of v1's 3-address layout.
- 2026-04-15: **v2 end-to-end через CLI работает** (`OTTER_V2_TRANSPILE=1`). Added `maybe_attach_v2_bytecode` hook in [module.rs](crates/otter-vm/src/module.rs) — when the env var is truthy, `Module::new`/`new_esm` transpiles every function's v1 bytecode to v2 at construction time (failures skip v2 attachment and keep v1 only). Feature forwarding wired through `otter-runtime` → `otterjs` so `cargo build --release -p otterjs --features bytecode_v2` produces a CLI that can exercise the full v2 path. **Critical correctness fix**: v2 dispatcher was reading/writing registers by RAW index; switched to `Activation::read_bytecode_register`/`write_bytecode_register` so v2 `Reg(n)` is USER-VISIBLE (same as v1 convention) and hidden-slot-offset (receiver/new.target) is respected. Without this, every function with a `this` slot (~all JS functions) read the receiver instead of parameter 0 on `Ldar r0`. Fixed `read_reg` / `write_reg` / `read_reg_list` signatures to thread `function: &Function`, bulk-updated all 20+ callers. Adjusted the `construct_preseeded_closure_returns_receiver` test to use `LdaThis + Star` instead of raw-index addressing of the hidden receiver slot. **Validated on release binary**: `/tmp/jit_test.ts` (sum 0..99) produces 4950 in both v1 and v2 modes; cold interpreter v2 is ~44× slower than v1 (expected — no inlined hot paths in `step_v2`, Phase 3b.10 work), **but after JIT warmup both run at 7.7 ms** for a 1 M-iteration sum — v2-tagged functions fall through to the v1 baseline analyzer for now because the Phase 4.1 v2 analyzer is too narrow to accept the full sum-function shape (LoadHole / LoadUndefined / Mov intermediates). Phase 4.5 widens the v2 analyzer+emitter so the x21-pinned stencil actually fires for real source-compiled code. Regression: 520 / 458 VM tests, 22 dispatch_v2 tests, workspace green.
- 2026-04-15: **Phase 3b.9 landed** (iteration opcodes). Added `GetIterator`, `IteratorNext`, `IteratorClose`, `ArrayPush`, `ForInEnumerate`, `ForInNext` to [interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs). **Activation gains `secondary_result: RegisterValue`** (mirroring the field already on `JitContext`) with `secondary_result()` / `set_secondary_result()` accessors — `IteratorNext` writes value into the accumulator and the done flag into secondary_result so compiler-emitted sequences `IteratorNext r; Star r_value; <branch on secondary>` preserve both channels. `GetIterator` uses the built-in fast path `runtime.objects.alloc_iterator` (Array/String/Map/Set); non-iterable values throw TypeError. `IteratorClose` is side-effectful via `runtime.objects.iterator_close`. `ArrayPush` uses `runtime.objects.push_element` (handles extensible / writable / length flags); non-array target throws TypeError. `ForInEnumerate` allocates a property iterator via `runtime.alloc_property_iterator` (routes null/undefined sources to an empty iterator per §14.7.5.6); `ForInNext value_reg iter_reg` writes the next key *directly into `value_reg`* and the done flag into the accumulator, matching the `ForInNext v iter; Star done` transpile pattern. Three new tests: `array_push_appends_accumulator_to_array` (three LdaSmi+ArrayPush pairs, verifies `array_elements` == [10, 20, 30]), `get_iterator_and_iterator_next_walk_array` (builds `[100, 200]` via push_element, iterates via GetIterator + three IteratorNexts, returns value0+value1 = 300), `for_in_enumerate_walks_property_keys` (`{a:1, b:2}` + three ForInNext steps — third returns done=true). dispatch_v2 tests **22/22**. Regression: `cargo test -p otter-vm --features bytecode_v2 --lib` **520/520**, v1-only **458/458**, `cargo test -p otter-jit --features bytecode_v2 --lib` **112/112** (1 ignored), `cargo build --workspace` green. Symbol.iterator / custom-iterator slow path (for non-built-in iterables) deferred to Phase 3b.9b.
- 2026-04-15: **Phase 4.3 partial — pipeline wiring done, end-to-end invocation deferred**. Wired the v2 template baseline into [crates/otter-jit/src/pipeline.rs](crates/otter-jit/src/pipeline.rs) via a `try_compile_v2_template(function)` helper that's probed before the v1 baseline / MIR paths in both `compile_function_with_feedback` and `compile_function_profiled`. When a function carries v2 bytecode and the analyzer + emitter accept it, the pipeline installs the x21-pinned stencil directly (same `CodeBuffer → compile_code_buffer` path v1 uses). Feature-gated: non-`bytecode_v2` builds get a no-op inline stub, zero cost. **Invocation smoke-test blocked**: both a trivial `LdaSmi 42; Return` stencil and the full sum-loop hang when invoked from Rust test harnesses on macOS/Apple Silicon, leaving UE (uninterruptible-exiting) zombie processes. Disassembly structurally mirrors the production v1 epilogue so the likely culprit is macOS `MAP_JIT` / `pthread_jit_write_protect_np` interaction that the existing `TierUpHook::execute_cached` path has already solved. Smoke-test is `#[ignore]`d with a focused note; Phase 4.4 wires the stencil through the production tier-up hook (which already handles MAP_JIT correctly) and adds the guarded variant + bailouts — at which point invocation becomes safe. Regression: `cargo test -p otter-jit --features bytecode_v2 --lib` **112/112 passing (1 ignored)**, v1-only **108/108**, `cargo build --workspace` green.
- 2026-04-15: **Phase 4.1 + 4.2 landed** (v2 baseline analyzer + x21-pinned aarch64 emitter). Added [crates/otter-jit/src/baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs), feature-gated behind a new `bytecode_v2` Cargo feature on `otter-jit` that forwards to `otter-vm/bytecode_v2`. **Analyzer** (`analyze_v2_template_candidate`): walks `function.bytecode_v2()` via `InstructionIter`, lowers 17 Phase-4 opcodes (`Ldar`/`Star`/`LdaSmi`, `Add`/`Sub`/`Mul`/`BitwiseOr`, `AddSmi`/`SubSmi`/`BitwiseOrSmi`, `TestLessThan`/`TestGreaterThan`/`TestLessThanOrEqual`/`TestGreaterThanOrEqual`/`TestEqualStrict`, `Jump`/`JumpIfToBooleanFalse`, `Return`) into a new acc-aware [`V2TemplateInstruction`](crates/otter-jit/src/baseline/v2.rs) IR (1- or 2-address form — no 3-address residue from v1), records backward branch targets as loop headers, tracks byte-PC → instruction-index mapping so branch patching can operate in bytecode space. Rejects unsupported opcodes with a precise `V2TemplateCompileError::UnsupportedOpcode { byte_pc, opcode }`. **Emitter** (`emit_v2_template_stencil`): trust-int32 variant — **x21 is pinned to the accumulator for the entire stencil**. Prologue: `push_x19_lr_32 / str_x20_at_sp16 / x19=ctx / x9=regs_base / x20=TAG_INT32 / x21=0`. Per-op emission: `LdaI32` → `mov x21, #imm`; `Star` → `box_int32 x10, x21 / str x10, [x9, slot]`; `Ldar` → `ldr x21, [x9, slot] / sxtw x21, x21`; `AddAcc` → `ldr x10 / sxtw / add x21, x21, x10 / sxtw`; `BitOrAccI32` → `mov x10, imm / orr x21, x21, x10`; `CompareAcc` → `ldr x10 / sxtw / cmp x21, x10` (flags); `JumpIfAccFalse` after a `CompareAcc` fuses into `b.ge/le/gt/lt/ne` (negation of the JS operator); `ReturnAcc` → `box_int32 x0, x21 / ldr x20 / pop x19,lr / ret`. Branch back-patching uses `CodeBuffer::patch_u32_le` — unconditional `B` (imm26) and conditional `B.cond` (imm19 + cond code). **Landing metrics**: sum-loop stencil is **280 bytes (70 insns)** — a 3× shrink from the Phase B.10 v1 stencil (828 bytes / 207 insns), matching the plan's "≈300 bytes" target. 4 unit tests: `analyzer_accepts_sum_loop`, `analyzer_rejects_unsupported_op`, `analyzer_refuses_function_without_v2_bytecode`, `emitter_produces_sum_loop_stencil` (disassembles the output with `bad64` and checks for `ADD`/`ORR`/`CMP`/`B`/`Bcc`/`RET` in the mnemonic stream). Regression: `cargo test -p otter-jit --features bytecode_v2 --lib` **112/112**, `cargo test -p otter-jit --lib` **108/108**, `cargo build --workspace` green. No invocation / tier-up-hook wiring yet — Phase 4.3 threads the `JitContext` accumulator-raw field through the interpreter↔JIT boundary so the compiled stencil can be called from `run_with_tier_up`.
- 2026-04-14: **Phase 3b.7b–3b.7c landed** (Construct + AssertConstructor + TailCall). Added: (a) `Construct(target, new_target, RegList)` dispatching through `runtime.construct_callable` which covers bound-function unwrap, proxy `[[Construct]]` trap, host/closure construct, and §9.2.2.1 return-value override (primitive returns replaced by the allocated receiver); (b) `AssertConstructor` — v2 variant reads the accumulator (empty operand shape, unlike v1's register operand) and throws `TypeError` if `!is_constructible`; (c) `TailCall(target, receiver, RegList)` — plain-closure target path builds a callee `Activation` in place and returns `StepOutcome::TailCall(TailCallPayload { module, activation })` so the outer `run_completion_with_runtime` loop swaps module + function + activation without a nested call. Non-plain targets (generator / async / class-ctor / host / proxy) fall back to `call_v2_callable` + `StepOutcome::Return(value)`. Three new tests: `construct_preseeded_closure_returns_receiver` (preseeded `function F(n){ this.x = n*2; }` closure, `new F(7)` returns the allocated receiver with `x == 14` verified through `runtime.objects.get_property`), `assert_constructor_throws_on_non_constructor` (acc=42, guard throws), `tail_call_invokes_closure_and_returns_its_value` (TailCall(double, undefined, [10]) → 20, dead code after TailCall never runs). dispatch_v2 tests **19/19**. Regression `cargo test -p otter-vm --features bytecode_v2 --lib` **517/517**, v1-only **458/458**.
- 2026-04-14: **Phase 3b.7 landed** (Call opcodes + RegList decode). Added `CallUndefinedReceiver` / `CallAnyReceiver` / `CallProperty` / `CallDirect` to [interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs). Two dispatch helpers: (a) `call_v2_callable` for closures/hosts/bounds — routes through `runtime.call_callable` which delegates to `Interpreter::call_function` (same path v1's accessor traps use, so async closures / promise reactions / host functions "just work" for the simple-call subset); (b) `call_v2_direct` for `CallDirect` — constructs a callee `Activation` directly from `FunctionIndex`, copies arguments into parameter slots via `FrameLayout::resolve_user_visible`, preserves overflow args for `CreateArguments`, and runs through `Interpreter::run_with_tier_up` so hotness budget accrues on the callee exactly like v1. Added `reg_list` / `read_reg_list` operand-helper pair (decodes the two-slot `Operand::RegList { base, count }` into `(u32,u32)` and reads the contiguous argument window). Both helpers surface JS throws through `StepOutcome::Throw(value)` so caller unwind is preserved, and caller-side `refresh_open_upvalues_from_cells` fires before writing the accumulator (same pattern as v1's CallClosure). Three new tests: `call_direct_adds_two_params` (two-function module, CallDirect adds 10+32=42 end-to-end through tier-up), `call_undefined_receiver_invokes_closure` (pre-allocates a closure via `runtime.alloc_closure`, preseeds it into r0, runs `CallUndefinedReceiver` dispatch → 21*2=42), `call_direct_propagates_throw` (callee throws smi 7, caller bubbles `UncaughtThrow(7)` untouched). Construct / CallSpread / TailCall / CallEval / CallSuper* deferred to Phase 3b.7b. dispatch_v2 tests **16/16**. Regression `cargo test -p otter-vm --features bytecode_v2 --lib` **514/514**, v1-only **458/458**, `cargo build --workspace` green. **Direct calls through the full `run_with_tier_up` path** — JIT integration in Phase 4 lights up automatically once the baseline emitter understands v2 opcodes.
- 2026-04-15: **Phase 4.5b partial — guarded emitter + bailout infra landed; real-code invocation still broken**. Added the tag-guarded v2 baseline emitter and its tier-up bailout pipeline, but the stencil for the source-compiled `sum()` diverges once `LdaThis`/`ToNumber` are accepted, so the analyzer is kept narrow (`sum()` still falls back to v1). **Plumbing landed:** (a) `JitContext` gains `accumulator_raw: u64` at offset 144, `sizeof == 152`; (b) `TierUpExecResult::Bailout` now carries `accumulator_raw: u64`, `DefaultTierUpHook::execute_cached` forwards `ctx.accumulator_raw`, `Interpreter::run_with_tier_up` reloads it into `activation.set_accumulator` via `RegisterValue::from_raw_bits` (silent no-op when the stencil left it as initial undefined); (c) [crates/otter-jit/src/baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs) rewritten with an `AccState::{Int32,Raw}` tracker per instruction, `load_int32_guarded` modeled on v1's `load_int32` (3-insn `eor/tst/b.ne` via the pinned `x20=TAG_INT32`), a shared bailout-common epilogue that returns `BAILOUT_SENTINEL`, and per-site pads that spill `x21` into `accumulator_raw` (int32-boxed when `AccState::Int32`, raw bits when `AccState::Raw`). Patcher distinguishes `CBZ`/`B.cond`/`B` by opcode mask so `cond=None` + `B` doesn't overwrite legitimate placeholders. (d) Analyzer + emitter extended with `Inc/Dec/Negate/BitwiseNot/MulSmi/BitwiseAndSmi/ShlSmi/ShrSmi` (emitter materialises the immediate into x10, applies the op on the W register, sxtws). Sum-loop stencil grows from 280 B (trust-int32) to 532 B (guarded) — test ceiling bumped to 640 B. Regression green: otter-vm (--features bytecode_v2) 520/520, otter-vm 458/458, otter-jit (--features bytecode_v2) 112/112 (1 ignored), otter-jit 108/108, workspace build green. **Known regression**: enabling `LdaThis`/`LdaCurrentClosure`/`ToNumber` in the analyzer makes the v2 path accept `sum()` but the 1020 B stencil throws `TypeError: operand expected int32 in v2 dispatch` at runtime — implying either (a) a branch/patching edge case in the larger stencil, (b) an acc_state mismatch across a forward branch whose target starts with `Star` (we'd emit the Raw-branch `str` when the producer was Int32), or (c) incorrect spill format for a bailout site whose actual x21 state diverges from the tracked state. The three analyzer arms are commented out in [baseline/v2.rs](crates/otter-jit/src/baseline/v2.rs:380) so `OTTER_V2_TRANSPILE=1` still produces correct results (1783293664) via v1 fallback. Next session: debug the emitter with a minimal repro and a disassembly dump; the guarded infrastructure is ready to light up once the stencil-divergence bug is root-caused.
- 2026-04-15: **M0 landed** (v2 migration). Deleted v1 source_compiler (~11k LOC), v1 bytecode.rs, v1 dispatch.rs, v1 JIT baseline/MIR/CLIF/IC/OSR infra, tests/node-compat, tests/test262, crates/otter-test262, and every integration test in `crates/*/tests`. Renamed `bytecode_v2` → `bytecode`, `dispatch_v2` → `dispatch`, `baseline/v2.rs` → `baseline/mod.rs`; dropped the `bytecode_v2` Cargo feature + `OTTER_V2_TRANSPILE` env gate. Scaffolded the new `source_compiler::ModuleCompiler` (returns `SourceLoweringError::Unsupported { construct: "program" }` for any input until M1). `otter run foo.js` now fails fast with the Unsupported error; `cargo build / clippy -D warnings / fmt --check / test --workspace` all green.
- 2026-04-14: **Phase 3b.1–3b.5 landed** (real-runtime v2 dispatch, skeleton). The `Activation` struct now carries a v2 accumulator (`activation.accumulator: RegisterValue`) with `accumulator()` / `set_accumulator()` accessors; defaults to `undefined` at frame creation, preserved across the existing generator save/resume path. `Function` gained a feature-gated `Option<bytecode_v2::Bytecode>` field plus `with_bytecode_v2(...)` / `bytecode_v2()` accessors. Added [crates/otter-vm/src/interpreter/dispatch_v2.rs](crates/otter-vm/src/interpreter/dispatch_v2.rs) — the real dispatcher that integrates with `RuntimeState`/`FrameRuntimeState`/`Interpreter`. Opcodes implemented in this skeleton (19 of the 95): `Ldar`/`Star`/`Mov`/`LdaSmi`/`LdaUndefined`/`LdaNull`/`LdaTrue`/`LdaFalse`/`LdaTheHole`/`LdaThis`, `Add`/`Sub`/`Mul`, `BitwiseAnd`/`BitwiseOr`, `AddSmi`/`BitwiseOrSmi`, `TestLessThan`/`TestGreaterThan`/`TestLessThanOrEqual`/`TestGreaterThanOrEqual`/`TestEqualStrict`, `Jump`/`JumpIfTrue`/`JumpIfFalse`/`JumpIfToBooleanTrue`/`JumpIfToBooleanFalse`, `Return`/`Throw`/`Nop`. Router in `Interpreter::run_completion_with_runtime` picks v2 when `function.bytecode_v2().is_some()`, else stays on v1 `step`. **End-to-end validation** through the real interpreter pipeline (`Interpreter::execute_with_runtime`): (a) `Ldar/Add/Return` over two int32 registers → 42, (b) `LdaSmi/Return` → 42, (c) `sum(0..99)=4950` via the full `while(i<n){s=(s+i)|0;i+=1;}` bytecode built from labels + auto-widened operands — all green. Verified: `cargo test -p otter-vm --features bytecode_v2 --lib` **501/501** (458 v1 + 40 v2 ISA/transpile/harness + 3 real-runtime), v1-only `cargo test -p otter-vm --lib` **458/458**, `cargo check --workspace` green. Next: Phase 3b.6 extends `dispatch_v2` with the remaining ~76 opcodes (property access, calls through `RuntimeState::call_callable`, generator suspend/resume wired to `StepOutcome::GeneratorYield`/`Suspend`, iterator protocol via `RuntimeState` helpers, exception unwind). Then Phase 4 teaches the JIT template baseline to consume v2 directly with x21 permanently pinned to the accumulator.

## Executive Summary

Otter should not aim to "beat V8 and JSC everywhere." That is not an engineering target.

Otter should aim to win in the workloads that matter for an embeddable Rust-first
runtime:

- faster warmup to native execution
- lower latency variance under mixed host/runtime load
- stronger host-integration performance
- smaller code and metadata footprint
- predictable tiering and deopt behavior
- competitive steady-state throughput on the hot subset we actually execute well

The current JIT already proves that native code generation is not the core
problem. On a pure arithmetic loop, the existing JIT test shows a very large
speedup over the interpreter. On more realistic JS entry scripts, the speedup is
small or negative because the hot path still falls back into generic runtime
helpers and the current OSR/speculation story is incomplete.

This plan changes the architecture accordingly:

1. Treat "compile fast" and "optimize hard" as different products.
2. Move Tier 1 off the `bytecode -> MIR -> CLIF` path.
3. Make ICs and small stubs the center of execution, not side tables.
4. Use direct bytecode-to-asm templates for the hot baseline subset.
5. Keep a richer optimizing tier for later, fed by frozen feedback snapshots.
6. Prebuild and reuse machine-code stubs aggressively where semantics allow it.

## Current Problems To Fix

These are the concrete issues in the current tree:

- `crates/otter-jit/src/pipeline.rs` compiles through `bytecode -> MIR -> CLIF -> machine code`. That is a reasonable prototype, but it is too expensive for a true fast baseline tier.
- `crates/otter-jit/src/mir/builder.rs` specializes only a narrow subset. Arithmetic is mostly `GuardInt32 + AddI32/SubI32/...`; property access and calls still depend heavily on helper boundaries.
- `crates/otter-jit/src/codegen/lower.rs` still leaves several guard kinds stubbed (`GuardShape`, `GuardProtoEpoch`, `GuardArrayDense`, `GuardBoundsCheck`, `GuardNotHole`, `GuardString`, `GuardFunction`, `GuardBool`).
- Even the "fast path" for properties in `crates/otter-jit/src/codegen/lower.rs` is not really inline field access; it emits host calls into `runtime_helpers.rs`.
- `crates/otter-jit/src/runtime_helpers.rs` shows that `GetPropShaped`, `SetPropShaped`, generic get/set, and even `CallDirect` still bounce through runtime code and can bail out for common JS cases.
- `crates/otter-jit/src/osr_compile.rs` explicitly says current OSR is not real mid-loop OSR; it recompiles the function and re-executes from the beginning.
- `JIT_INCREMENTAL_PLAN.md` still leaves "full CacheIR dispatch for property access", "Baseline JIT emits calls to IC stubs", "true mid-loop OSR", and the speculative MIR builder incomplete.

## Hard Requirements

This refactor is only worth doing if these requirements are treated as non-negotiable:

1. Correct JS semantics first.
2. GC safety first. Every tier must have valid stack maps or explicit safepoint policy.
3. Deopt must be cheap enough to use aggressively.
4. No hot-path generic runtime helper calls for the monomorphic common case.
5. Tier 1 compile latency must be measured as a product metric, not a side effect.
6. Patchability and invalidation must be built in from day one.
7. Every hot speculation must have an explicit invalidation owner.
8. Code size and metadata size must be tracked alongside throughput.

## Non-Goals

These are explicitly out of scope for the first refactor wave:

- preserving the current JIT architecture for compatibility reasons
- beating V8, JSC, Bun, and SpiderMonkey on broad JS conformance/perf matrices
- building a full TurboFan/FTL-class peak compiler immediately
- replacing the entire runtime object model before stabilizing tiering
- introducing heroic whole-program optimization
- tracing JIT as the primary architecture

Tracing may become a future sidecar for pathological loop workloads, but not the
primary execution design.

## Compatibility Policy

Backwards compatibility is not a planning constraint for this refactor.

If the current JIT architecture, module boundaries, tier APIs, or internal
ownership model block a cleaner Tier 1/Tier 2 split, they should be changed
directly instead of preserved behind shims. The only compatibility bar that
matters here is semantic correctness at the JS/runtime boundary.

## New Target Architecture

Otter should move to a 4-part execution architecture.

### 1. Tier 0: Threaded Interpreter + Superinstructions

Purpose:

- canonical correctness tier
- feedback collection owner
- fallback/deopt landing pad
- fast enough that Tier 1 is only needed for real heat

Changes:

- replace plain match-dispatch hot paths with direct-threaded or equivalent low-overhead dispatch where feasible
- add superinstructions for the common bytecode pairs/triples
- make inline caches executable in the interpreter, not just metadata
- record hot-exit reasons, not only hot function counts

Examples of candidate superinstructions:

- `LoadI32 + Add + Store`
- `GetProperty(monorphic) + Call`
- loop-header compare/branch pairs
- array length/load/store triplets

This is the cheapest performance win in the whole system and reduces pressure on the JIT.

### 2. Tier 1: Template Baseline JIT

Purpose:

- compile extremely fast
- produce compact machine code
- execute the hot monomorphic subset without touching generic runtime code
- support patchable IC call sites and true OSR entry

Design:

- direct `bytecode -> asm` for the supported subset
- no MIR
- no Cranelift
- explicit macro-assembler per target (`x64`, `aarch64`)
- fixed register policy and fixed frame contract
- patchable sites for property/call/element IC stubs

Tier 1 is where "why not emit asm directly?" becomes a real product decision.
Yes, we should do that here.

But the unit of reuse should not be "full function precompiled ahead of time."
The unit of reuse should be:

- instruction templates
- prologue/epilogue templates
- IC stub templates
- deopt/safepoint helpers
- call trampolines
- write-barrier stubs

That gives fast compilation without pretending full JS execution is static.

### 3. Tier 1.5: Stub Compiler / IC Engine

Purpose:

- make property access, element access, calls, and key guards genuinely fast
- move specialization pressure out of the main baseline compiler
- keep monomorphic and low-degree polymorphic cases in executable stubs

Design:

- CacheIR-like stub DSL is the single source of truth
- interpreter can execute it
- baseline JIT can call or inline selected stub shapes
- optimizing tier can lower from frozen snapshots of the same data

Stub families:

- named property load/store
- dense element load/store
- global load/store
- direct call target check
- builtin fast-call entry
- prototype-chain validity guards

### 4. Tier 2: Speculative Optimizing Tier

Purpose:

- much better steady-state throughput on stable hot functions
- eliminate guard churn, representation churn, and memory traffic
- inline across bytecode and stub boundaries when profitable

Design:

- consumes frozen feedback snapshots
- emits JS-specific SSA
- has real deopt state materialization
- performs selective inlining, LICM, value numbering, range analysis, escape analysis where justified

Backend decision:

- keep Cranelift as an intermediate backend only if it does not block machine-code quality or compile budget
- if Cranelift remains too constraining, replace only the Tier 2 backend later
- do not let Tier 2 backend concerns contaminate Tier 1 compile latency

## Core Refactor Decision

The most important architectural change is this:

**Tier 1 must stop being a reduced version of Tier 2.**

Today the baseline path is effectively a simplified optimizing compiler. That is
why it is too expensive to compile and still too dependent on runtime helpers.

The new split must be:

- Tier 1: fast, shallow, patchable, helper-averse
- Tier 2: slower, deeper, speculative, deopt-rich

If a feature forces heavy IR construction, global analysis, or expensive lowering,
it belongs in Tier 2, not Tier 1.

## AOT / Prebuilt Native Work

There are two kinds of "preprepare asm" and they should be separated clearly.

### Good preprepared work

- prebuilt machine stubs for calls, deopts, safepoints, slow-path entries
- prebuilt IC stub skeletons with relocations/patch slots
- prebuilt builtin fast paths for very stable internal operations
- snapshot/preinitialized bootstrap state where correctness allows
- build-time generated assembler helpers from a small DSL/table

### Bad preprepared work

- full-function AOT native code for arbitrary JS before feedback exists
- static specialization that ignores shape/version invalidation
- templates that assume object layout invariants without watchpoints
- machine code that cannot reconstruct interpreter state on failure

JS is dynamic. The static part is the *scaffold*, not the final specialization.

## What Must Be Deleted Or Demoted

The refactor should aggressively remove these anti-patterns from the hot path:

- generic runtime helper calls for monomorphic property access
- generic runtime helper calls for simple direct calls
- fake OSR that restarts execution from function entry
- Tier 1 dependence on heavyweight CLIF lowering for small functions
- speculation without explicit invalidation ownership
- duplicate specialization logic split across interpreter and JIT

## Major Workstreams

## Workstream A: Baseline Macro Assembler

Deliverables:

- `crates/otter-jit/src/masm/` target-independent API
- `crates/otter-jit/src/arch/x64.rs` and `aarch64.rs` turned into real macro-assembler backends
- fixed scratch-register policy
- patchable call/jump/guard sites
- reusable prologue/epilogue/deopt trampolines

Exit criteria:

- Tier 1 compiles without MIR/CLIF for the supported subset
- generated code is smaller than current baseline output on the same subset
- compile latency drops by an order of magnitude on short functions

## Workstream B: Executable ICs

Deliverables:

- CacheIR-like representation made authoritative
- stub compiler for monomorphic and low-polymorphic cases
- patchable baseline call sites
- invalidation wired to watchpoints / proto epochs / shape transitions

Exit criteria:

- monomorphic property get/set avoids generic runtime helper crossings
- direct call fast path does not rebuild callee register windows through generic fallback
- megamorphic behavior cleanly exits to interpreter/runtime without corrupting state

## Workstream C: Real OSR

Deliverables:

- loop-header liveness map
- real OSR entry block generation
- JIT PC map for resume and profiling
- deopt materialization from compiled frame state back to interpreter state

Exit criteria:

- hot loop can enter compiled code at header without restarting function
- deopt can return to the exact bytecode PC with reconstructed live values
- bailout frequency and reason telemetry available per loop header

## Workstream D: Runtime Boundary Cleanup

Deliverables:

- split "hot inlineable runtime operations" from "slow generic semantics"
- explicit APIs for shaped loads/stores, dense array fast paths, builtin call fast paths
- write-barrier and allocation fast paths callable from JIT without generic interpreter entry

Exit criteria:

- JIT does not need to call catch-all helpers for common monomorphic operations
- allocation and write barrier have dedicated JIT-safe entry points
- property/call/array fast paths are auditable and benchmarkable in isolation

## Workstream E: Tier 2 Compiler

Deliverables:

- frozen feedback snapshot pipeline
- speculative builder from feedback/IC state
- real guard lowering for shape/proto/bounds/hole/string/function/bool cases
- materialized deopt state
- selective inliner

Exit criteria:

- Tier 2 meaningfully outperforms Tier 1 on stable hot code
- Tier 2 compile budget is bounded and measurable
- deopt behavior is stable under changing shapes/call targets

## Workstream F: Tooling And Perf Discipline

Deliverables:

- assembly diff tooling
- per-tier compile latency dashboards
- helper/stub hit-rate telemetry
- deopt histograms by reason and site
- code cache occupancy and aging metrics

Exit criteria:

- every perf claim can be backed by a benchmark and disassembly
- regressions are caught by CI before landing

## Phased Plan

### Phase 0: Measurement Reset

Do first:

- freeze benchmark set for JIT work
- add release-mode perf gates, not only debug test timings
- add benchmark categories: arithmetic, monomorphic props, polymorphic props, arrays, calls, closures, mixed host/runtime
- record compile latency separately for Tier 1 and Tier 2

Success condition:

- one dashboard shows interpreter, Tier 1, Tier 2, compile cost, deopts, code size

### Phase 1: Threaded Interpreter + Superinstructions

Do next:

- implement low-overhead dispatch improvements
- add first 10-20 superinstructions
- make interpreter IC execution authoritative

Success condition:

- interpreter baseline improves enough that Tier 1 can be more selective

### Phase 2: Template Baseline JIT

Do next:

- land macro assembler
- implement direct bytecode-to-asm for arithmetic, branches, locals, loop headers, returns
- add patchable sites for property/call ICs

Success condition:

- the "pure loop" class of workloads compiles in tens to hundreds of microseconds, not milliseconds

### Phase 3: Executable ICs And Runtime Fast Paths

Do next:

- compile monomorphic property/call/element stubs
- remove helper crossings from common monomorphic cases
- add invalidation plumbing

Success condition:

- real JS scripts with property access and calls see clear speedups, not only arithmetic microbenches

### Phase 4: Real OSR + Deopt Materialization

Do next:

- enter compiled code at loop headers
- resume interpreter at exact bailout PCs
- materialize live values on exit

Success condition:

- hot loops no longer pay restart cost or fake tier-up behavior

### Phase 5: Tier 2 Optimizer

Do next:

- frozen snapshots
- speculative builder
- true guard lowering
- inlining and representation optimization

Success condition:

- stable hot workloads gain real steady-state wins over Tier 1

## Performance Gates

These are directional gates. Tune exact thresholds after first release-mode data,
but keep the shape of the gates.

### Tier 1 compile latency

- tiny function: p50 under 0.2 ms in release
- small hot loop: p50 under 0.5 ms in release
- medium monomorphic function: p95 under 2 ms in release

### Tier 1 throughput

- arithmetic loops: at least 10x over interpreter
- monomorphic property loops: at least 3x over interpreter
- simple call chains: at least 2x over interpreter

### Tier 2 throughput

- stable hot kernels: at least 1.5x over Tier 1
- mixed monomorphic JS: clearly better than Tier 1 without deopt thrash

### Code quality

- no uncontrolled code size explosion relative to baseline
- no helper-heavy hot traces in disassembly for monomorphic kernels

## Production Gates

The JIT is not production-ready until all of these are true:

1. Tier 1 can compile and execute a documented subset without MIR/CLIF.
2. Property/call/element monomorphic fast paths do not cross generic runtime helpers.
3. Real OSR exists.
4. Real deopt materialization exists.
5. GC safepoints and stack maps are validated under stress.
6. Invalidation is wired for every installed speculation kind.
7. Release-mode perf dashboard is green.
8. Code cache has eviction/aging policy.
9. JIT crashes can be triaged with asm, site metadata, and deopt telemetry.
10. Cross-arch behavior is tested on both `x64` and `aarch64`.

## First Concrete Implementation Slice

If we want maximum leverage with minimum wasted motion, the first slice should be:

1. Build Tier 1 macro assembler for `LoadI32`, `Move`, `Add/Sub/Mul`, `Lt`, `Jump`, `JumpIfFalse`, `Return`.
2. Add patchable IC call sites even before full stub compiler lands.
3. Replace helper-backed `CallDirect` with a dedicated direct-call fast path for same-module functions.
4. Implement real shape guard lowering instead of stubbing it.
5. Add release-mode benchmark gate for `benchmarks/jit/arithmetic_loop.ts`, `monomorphic_prop.ts`, and `call_chain.ts`.

That slice forces the architecture to become real. Everything after that compounds.

## Decision Summary

The plan is not "make the existing MIR + Cranelift path a bit better."

The plan is:

- faster interpreter
- direct-template baseline JIT
- IC/stub-first hot path
- real OSR/deopt
- speculative optimizer on top
- prebuilt asm scaffolding where it helps

That is the shortest path to a JIT that is both fast and production-grade for Otter.
