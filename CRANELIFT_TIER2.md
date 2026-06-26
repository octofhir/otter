# Cranelift Tier2 Backend — Design Plan

**Status:** planning only, no implementation.
**Updated:** 2026-06-26
**Owner doc:** extends [`JIT_DESIGN.md`](./JIT_DESIGN.md) "Post-Tier1 Roadmap → Cranelift backend".

This document plans a Cranelift-based code generator as a **second backend for
the existing DiveJIT optimizing tier**, not a new compiler tier and not a
replacement for correctness work. It is a backend swap: the same typed SSA graph
that `crates/otter-jit/src/optimizing/` already builds is lowered to Cranelift IR
instead of hand-emitted arm64, so Cranelift owns instruction selection, register
allocation, and multi-platform codegen.

No code lands from this document. Implementation is gated behind Tier1 closure
(see "Sequencing").

---

## 1. Why Cranelift, why now

### Verified Tier1 baseline (2026-06-26, `target/release/otter`, `OTTER_JIT=1`)

Min wall-clock ms, otter vs Node v24.16.0 (`benchmarks/bench.mjs`):

| script | otter | node | node × | class |
| --- | --- | --- | --- | --- |
| mandelbrot.js | 29.5 | 31.7 | **0.93×** | closed |
| regex.js | 26.0 | 32.8 | **0.79×** | closed |
| array-ops.js | 133.8 | 93.9 | 1.43× | hold |
| sort.js | 253.7 | 162.6 | 1.56× | hold |
| nbody.js | 71.2 | 31.9 | 2.23× | **codegen-bound** |
| fib.js | 96.5 | 41.4 | 2.33× | **codegen-bound** |
| typed-array.js | 98.5 | 36.3 | 2.71× | **codegen-bound** |
| typescript-sample.ts | 178.0 | 55.3 | 3.22× | mixed |
| json.js | 872.2 | 234.1 | 3.73× | runtime subsystem |
| string-ops.js | 142.9 | 36.0 | 3.96× | runtime subsystem |
| prop-access.js | 158.9 | 40.5 | 3.93× | mixed (IC + codegen) |
| tree-traversal.js | 572.4 | 45.4 | 12.62× | runtime subsystem (alloc/GC) |

**Reading:** mandelbrot and regex already beat Node. The residual 2–3× on
`fib`/`nbody`/`typed-array` is *codegen quality* — pure-compute loops where the
hand-emitted arm64 tier runs out of instruction-selection and register-allocation
headroom (7 GP / 6 FP regs, linear-scan, no peephole/ISel beyond hand-written
patterns). `json`/`string`/`tree-traversal` are runtime-subsystem (allocation,
native builtins, GC throughput) and are **not** addressed by a backend swap.

Cranelift targets exactly the codegen-bound class: production register allocator
(backtracking), real instruction selection (ISLE), constant folding, and
cross-platform (x86_64 + aarch64) output from one IR.

### What Cranelift does *not* buy us
- It does not fix `json`/`string`/`tree-traversal` (runtime/GC work — separate).
- It does not replace exact-PC deopt, GC rooting, IC guards, or builtin
  semantics. Those remain Otter-owned (Section 4).
- It is not faster than the dynasm tier on day one. Keep dynasm until Cranelift
  is measured faster *and* passes the same gates (Section 8).

---

## 2. Where Cranelift plugs in

The backend boundary is already a clean trait — `crates/otter-vm/src/jit.rs`:

```
trait JitFunctionCode: Debug + Send + Sync {
    fn code_len(&self) -> usize;
    fn osr_only(&self) -> bool;
    fn entry_addr(&self) -> Option<usize>;
    fn run_entry(&self, ptrs: JitReentryPtrs) -> JitExecOutcome;
    fn osr_entry(&self, ptrs: JitReentryPtrs, byte_pc: u32) -> Option<JitExecOutcome>;
}
```

`JitReentryPtrs { vm, stack, context, frame_index }` (erased `*mut Interpreter`,
`*mut JitFrameStack`, `*const ExecutionContext`).
`JitExecOutcome = Returned(Value) | Bailed(byte_pc) | Threw(VmError)`.

Today there are two implementers: `baseline::*` and
`optimizing::emit::OptimizedCode`. Cranelift adds a **third** implementer,
`optimizing::clif::CraneliftCode`, fed by the *same* upstream passes:

```
JitFunctionView
   └─ optimizing::build_graph / build_osr_graph   (ir::Graph)   [unchanged]
   └─ optimizing::deopt::capture_* (frame states, OSR entries)  [unchanged]
   └─ optimizing::liveness + regalloc                           [dynasm path only]
   ├─ emit::emit  → OptimizedCode      (existing arm64 dynasm backend)
   └─ clif::emit  → CraneliftCode      (NEW: Graph → Cranelift IR → JITModule)
```

Cranelift replaces `liveness` + `regalloc` + `emit` for its path: Cranelift does
its own SSA value numbering, liveness, and allocation. We keep
`deopt::capture_*` because those produce **interpreter-visible** frame state
(bytecode PC + live register map), which Cranelift has no concept of and which is
the source of truth for bail/OSR.

**Crate placement:** new module `crates/otter-jit/src/optimizing/clif/`
(`mod.rs`, `lower.rs`, `deopt.rs`, `abi.rs`, `runtime.rs`). `cranelift-codegen`,
`cranelift-frontend`, `cranelift-jit`, `cranelift-module` as deps of
`otter-jit` only. No new dep edges into parked shims (CLAUDE.md rule).

---

## 3. SSA graph → Cranelift IR lowering

`ir::Graph` is already typed SSA with explicit `Repr` (`Tagged`/`Int32`/`Float64`/`Bool`).
Mapping is mostly mechanical:

| DiveJIT `Repr` | Cranelift type |
| --- | --- |
| `Int32` | `i32` |
| `Float64` | `f64` |
| `Bool` | `i8` (or `b1` block-arg predicate) |
| `Tagged` | `i64` (NaN-boxed `Value` bit pattern, opaque to CLIF) |

| DiveJIT `NodeKind` | CLIF lowering |
| --- | --- |
| `Param(n)` | block param of entry block / loaded from register window |
| `ConstInt32`/`ConstF64`/`ConstBool` | `iconst`/`f64const`/`iconst` |
| `Int32Add/Sub/Mul` | `iadd`/`isub`/`imul` + overflow check → deopt side exit |
| `Float64Add/...` | `fadd`/`fsub`/`fmul`/`fdiv` |
| `Int32Compare`/`Float64Compare` | `icmp`/`fcmp` → `Bool` |
| `CheckInt32` | tag test on `Tagged`; on fail `brif` → deopt block |
| `CheckShape` (property feedback) | load shape ptr, `icmp`, `brif` → deopt block |
| `Phi` | CLIF block parameters (Cranelift is block-param SSA, not phi nodes) |
| `Box`/`Unbox` edges | NaN-box compose / tag-strip inline sequences |

**Block/phi model:** DiveJIT `Phi(Vec<NodeId>)` aligned with `block.preds` maps
directly onto Cranelift block parameters — pass phi inputs as branch args on each
predecessor edge. This is structurally *cleaner* in CLIF than in the current
hand-emitter (which materializes phi moves on edges manually in `emit.rs`).

**Control flow:** the `Unsupported::ControlFlow` variant in
`optimizing/mod.rs` is already dead (the dynasm emitter lowers multi-block
graphs via `Terminator::Branch`). Cranelift inherits the full CFG; no regression
in coverage.

---

## 4. The hard part: deopt on a backend with no deopt

Cranelift has **no native deoptimization**. V8/Maglev deopt is a first-class IR
concept; CLIF has nothing equivalent. We must model every speculation guard as an
explicit **side exit** that reconstructs interpreter state and returns
`Bailed(byte_pc)` — reusing the existing `JitExecOutcome::Bailed` contract.

### Strategy: materialize-and-return side-exit blocks
For each guard, lower to:

```
  v_ok = <guard predicate>            ; e.g. icmp tag == int32
  brif v_ok, continue_blk, deopt_blk_k(<live SSA values>)
deopt_blk_k(a, b, c, ...):            ; one cold block per guard PC
  ; write each live interpreter register to the frame register window
  ; (frame base from JitReentryPtrs.stack + frame_index, stable address)
  store.i64 boxed_a, [regwin + off_ra]
  store.i64 boxed_b, [regwin + off_rb]
  ...
  ; return JitExecOutcome::Bailed(byte_pc)
  return Bailed(byte_pc)
```

The frame-state metadata (`deopt::capture_frame_states`) already tells us, per
guard byte-PC, **which interpreter registers are live and how each maps to an SSA
value + repr**. We reuse it verbatim — Cranelift's only job is to keep those SSA
values available at the side-exit block (it will, because the block consumes them
as block args, extending their live ranges exactly as `deopt_value_uses` does for
the dynasm allocator).

**Boxing at the exit:** unboxed `Int32`/`Float64` values must be NaN-boxed back
into `Value` bit patterns before the store, identical to the dynasm path's
`box`-on-deopt. Emitted inline in the deopt block.

**Why side-exit and not a real deopt API:** it preserves the existing
interpreter-as-oracle model with zero new VM surface — the VM already handles
`Bailed(pc)` by resuming the interpreter at the exact instruction with committed
side effects intact. No interpreter changes needed.

**Cost note:** deopt blocks are cold; mark them `cold` so Cranelift lays them out
of the hot path and does not penalize the fast path's register allocation.

### OSR entry
`deopt::capture_osr_entries` gives loop-header resume points. Cranelift supports
multiple entry points poorly within one function, so OSR is a **separate
compiled artifact** per OSR pc (as today: `osr_only()` codes), whose entry block
loads all live registers from the frame window into SSA values, then joins the
loop header block. One `CraneliftCode` per `(function, osr_pc)`.

---

## 5. ABI and runtime bridge

`run_entry(JitReentryPtrs) -> JitExecOutcome` is the native entry signature.
Plan:

- Compile each function as a CLIF function with signature
  `fn(vm: i64, stack: i64, context: i64, frame_index: i64) -> i64` where the
  return packs the `JitExecOutcome` discriminant + payload (or returns a pointer
  to an out-param struct; decide during impl — out-param avoids packing `Value`).
- Reuse the **existing bridge methods** for re-entry: `Call`/`CallMethod`/
  `MakeFunction` lower to a CLIF `call` of the same `extern "C"` thunks the
  dynasm tier already calls (`jit_runtime_call`, `jit_runtime_make_function`).
  These are declared as Cranelift external functions (`Module::declare_function`
  + `colocated=false`, real addresses via `JITModule` symbol lookup).
- The frame register window is addressed off `stack + frame_index` exactly as the
  dynasm emitter does; the `HoltStack` stable-address invariant
  (`jit.rs` `JitFrameStack`) is what makes a held frame pointer survive re-entry.

**Direct calls:** `entry_addr()` must return the Cranelift-compiled entry so
compiled→compiled direct branches keep working across backends. Cranelift
`JITModule::get_finalized_function` gives the address.

---

## 6. Safepoints / StoneMaps (GC) — staged

Tier1 deliberately keeps GC-bearing pointers out of machine registers across
safepoints (boxed values stay in the frame window, which the VM traces). The
first Cranelift cut **keeps this discipline**: no boxed `Value` lives only in a
CLIF register across a call; reload from the frame window after any re-entry.
This means **no Cranelift stackmaps required for the initial backend** — same
GC contract as the current optimizing tier.

Precise stackmaps (Cranelift `enable_safepoints` + `func.stack_maps`) are a
**later** item, tied to the DeepDive tier and "StoneMaps" work in JIT_DESIGN.md,
where keeping boxed values in registers across calls becomes a perf lever. Plan
it, don't build it in the first cut:

- map Cranelift's reference-typed stackmap slots back into Otter trace roots;
- validate under `OTTER_GC_STRESS=128`;
- share metadata with deopt frame states where the safepoint and a guard coincide.

---

## 7. Staging (each stage independently shippable + gated)

The full-Maglev-grade discipline ([[feedback_no_simplifications_maglev]]) applies:
no bail-to-entry, exact-PC deopt from day one. Stages widen *opcode/shape
coverage*, never weaken the *algorithm*.

- **S0 — Skeleton.** Cranelift deps wired; `clif::emit` compiles the simplest
  numeric single-loop graph (the `fib`/`mandelbrot` subset) end to end, with real
  side-exit deopt. Behind the existing tier selection — *not* a feature flag; it
  is the optimizing backend choice, chosen by a single code path. Verify CLIF
  path `OTTER_JIT=1` matches dynasm path bit-for-bit on `fib`/`mandelbrot`/`nbody`.
- **S1 — Full numeric + control flow.** All `Int32`/`Float64`/`Bool` nodes,
  multi-block CFG, phis as block params, OSR artifacts. Target: match or beat
  dynasm on `fib`/`nbody`/`typed-array`.
- **S2 — Property IC + method inline.** `CheckShape` guards, inline slot
  load/store, monomorphic method inline (port the `property_feedback` /
  inline-method logic from `builder.rs`). Target: move `prop-access`.
- **S3 — Calls + direct calls.** `Call`/`CallMethod`/self-recursion via bridge
  thunks; `entry_addr` direct branches across backends.
- **S4 — x86_64.** Turn on the second Cranelift target; differential-test the
  whole bench + Test262 subset on x86_64 (CI runner or local cross-check).
- **S5 — Cutover decision.** When Cranelift is measured faster on the
  codegen-bound class *and* passes all gates on both arches, make it the default
  optimizing backend; keep dynasm as the arm64 fallback until proven redundant.

DeepDive-tier optimizations (BCE, LICM, escape analysis, polymorphic IC,
stackmaps) are **out of scope** for this document — they ride on top of a stable
Cranelift backend, per JIT_DESIGN.md.

---

## 8. Verification contract (per stage)

Inherit JIT_DESIGN.md's contract, plus backend-diff gates:

- `cargo build --release -p otter-cli`; `cargo test -p otter-jit`;
  `cargo test -p otter-vm` on VM-touching changes.
- **Backend parity:** for every touched workload, CLIF backend output ==
  dynasm backend output == `OTTER_JIT=0` interpreter output (three-way diff).
- **No regression** vs the prior committed baseline on `mandelbrot`, `nbody`,
  `fib`, `typed-array` (the protected set).
- `OTTER_GC_STRESS=128 OTTER_JIT=1` on touched workloads.
- Test262 subset for touched opcodes/builtins, JIT-on/off **failing-set
  parity** (the standing conformance proof).
- Debug-marker grep before commit (same list as JIT_DESIGN.md).

---

## 9. Risks / open questions

- **Deopt density.** Many cold side-exit blocks could bloat code size and slow
  Cranelift compile time. Mitigate with `cold` block flags; measure compile
  budget (S0 gate includes compile-time-per-function).
- **`JitExecOutcome` return packing.** `Value` is 64-bit NaN-boxed; `Threw`
  carries a `VmError` (fat). Likely return via caller-provided out-param pointer
  rather than packing into one i64. Resolve in S0.
- **Cranelift version churn.** Pin `cranelift-*` exact versions; treat upgrades
  as gated changes (codegen output can shift).
- **Compile latency vs dynasm.** Cranelift opt pipeline is slower to compile than
  dynasm. Tier-up threshold and possibly a "compile in background thread" policy
  may be needed; out of scope here but flagged.
- **OSR multiplicity.** One compiled artifact per OSR pc could multiply compiles
  for loop-dense functions. Measure; consider a single multi-entry artifact if
  Cranelift's entry-block story improves.

---

## 10. Source map

- Backend seam: `crates/otter-vm/src/jit.rs` (`JitFunctionCode`, `JitReentryPtrs`,
  `JitExecOutcome`).
- Reused upstream: `crates/otter-jit/src/optimizing/{builder.rs,ir.rs,deopt.rs}`.
- New: `crates/otter-jit/src/optimizing/clif/{mod,lower,deopt,abi,runtime}.rs`.
- Existing backend to mirror semantics from: `crates/otter-jit/src/optimizing/emit.rs`.
- Bridge thunks to reuse: `Interpreter::jit_runtime_call`,
  `jit_runtime_make_function` (`crates/otter-vm/src/`).
