# Otter optimizing tier — design + staged plan

Follow-up to [`ENGINE_ARCH_REWORK.md`](./ENGINE_ARCH_REWORK.md) **D5** ("codegen is
100% memory-bound: no CPU register allocation"). The baseline emitter
(`crates/otter-jit/src/baseline.rs`) is a one-linear-pass Sparkplug-style tier:
every operand is `ldr X,[x19,off]`, every result `str X,[x19,off]`, and a value
produced by one op is re-loaded and re-decoded by the next. mandelbrot sits
~75% in its own compiled loop yet still ~7× node because `x`/`y` round-trip
memory (and the NaN-box decode/encode) on every use.

This document is the plan to remove that ceiling **incrementally, inside the
existing baseline emitter**, without a separate IR or a second tier first. Each
slice is independently shippable and gated (test262 failing-set diff vs the
pre-slice baseline + bench deltas + JIT-on parity).

## Why this can be done in-place and GC-safely

The baseline tier's GC contract (`baseline.rs` module docs) is: *no JS value is
ever live in a machine register across a `Call`/`MakeFunction` safepoint*, so
the tier needs no GC stack maps — GC traces only the frame's rooted register
array. Residency optimization must not break that contract. The wedge:

> **An unboxed `int32` or `f64` is not a GC pointer.** Holding a decoded number
> in a CPU/FP register across ops cannot create a dangling reference, because a
> moving collection never rewrites a raw double. Only *boxed pointer* Values are
> GC-relevant.

So the safe subset for register residency is **unboxed numbers**. Every
optimizing slice below stays within it: pointer-typed values keep going through
memory exactly as today.

## Coherence model — write-through, advisory cache

Slice 1 keeps the frame slot in memory **authoritative and always current**
(write-through). The FP register holding a slot's decoded `f64` is a pure read
cache:

- A float-arith result is boxed and `str`'d to its slot exactly as today **and**
  its decoded `f64` is parked in a callee-saved FP register, recording
  `slot -> dreg`.
- A later op that needs that slot as a double reads the parked `dreg` instead of
  `ldr` + `emit_num_to_double` (decode).
- Because memory is always coherent, a guard **bail** or a **safepoint** that
  reads the frame array sees correct values with no flush required. The cache is
  advisory: dropping any entry is always sound, so conservative invalidation can
  never be wrong — only less fast.

Later slices (deferred stores, unboxed phi across the loop back-edge) relax
write-through and therefore must flush at boundaries; they come after slice 1
proves the residency bookkeeping.

## Invalidation (block boundaries)

The residency map is cleared (entries dropped, nothing to flush under
write-through) at every point where straight-line reasoning breaks:

- **Function entry** and every **branch target** (an instruction reachable by a
  jump — incoming register state is unknown). Target set = every `byte_pc`
  named by a branch operand, computed in the pre-scan that already finds
  `loop_headers`.
- Immediately **before** any `Call` / `MakeFunction` / delegate / runtime bridge
  (safepoint) and after it returns.
- Before **bail** / **return**.
- On **store** to a slot: that slot's entry is dropped (and any alias).
- Any op outside the modelled numeric set drops the whole map (conservative).

## FP register file

Use `d8`–`d15` (AAPCS callee-saved low 64 bits). Any function that parks a value
in `d8`–`d15` saves/restores the used subset in prologue/epilogue. A tiny pool
with round-robin eviction (drop the oldest `slot -> dreg`) covers chained
arithmetic; pressure beyond 8 simply evicts (write-through means an evicted slot
is still correct in memory).

## Staged plan

Each stage: implement → `just test262 --filter` over arithmetic/control suites
+ full Array/numeric dirs both JIT modes → failing-set diff vs pre-stage →
bench deltas (mandelbrot/nbody/fib) → commit.

- **S1. Write-through unboxed-`f64` read cache.** Park float-arith results in
  `d8`–`d15`; float consumers (`Add/Sub/Mul/Div/Rem` float path, `cmp` float
  path) read the parked reg instead of `ldr`+decode. Boundary invalidation as
  above. Memory authoritative throughout. *Target: mandelbrot/nbody inner-loop
  decode/reload elided; first measurable D5 win.*
- **S2. Unboxed `int32` residency.** Same for the int fast paths (`w`-reg
  cache), covering integer loop counters and fib's `n`.
- **S3. Deferred stores.** Drop write-through for slots provably re-read before
  any boundary; flush dirty residents at each boundary. Removes the producer
  `str` too. Requires the flush discipline S1 deliberately avoids.
- **S4. Loop-carried residency.** Keep a loop counter / accumulator in a fixed
  reg across the back-edge (reconcile at the header), so the value never touches
  memory inside the loop. The real LICM/regalloc payoff.
- **S5. Unboxed value stack / linear-scan allocation.** Generalize the ad-hoc
  pool into a proper live-range allocator over the linear pass — the umbrella
  D5 lever. At this point a separate SSA IR may pay for itself; revisit then.

Regex stays out of scope throughout.

_Started 2026-06-19 on `perf/engine-rewrite`. Reproducers: `benchmarks/micro/`,
`benchmarks/scripts/{mandelbrot,nbody,fib}.js`. Profiles: `benchmarks/profiles/`._

---

# Part II — a separate optimizing tier (Maglev-analog)

The in-place slices above (S1) raised the baseline's ceiling but cannot reach
it: the baseline is by construction a single linear pass with no IR, no register
allocation, and no deopt, so LICM / linear-scan regalloc / speculative inlining
have nowhere to live. S1 (`d593463f`, write-through f64 read cache gated on
`Op::Div`, ~7% mandelbrot) is its practical limit and the new tier subsumes it.

This part is a **second compiled tier** living in `crates/otter-jit/src/optimizing/`,
selected by hotness *above* the baseline in the existing tier-up ladder (not an
env flag — the new path is the default and is reverted via git, never a
kill-switch). It builds a typed SSA graph from bytecode for a hot function,
speculates representations from interpreter type feedback, lowers to unboxed
arm64 with linear-scan register allocation, and deoptimizes to the
interpreter/baseline when a guard fails. The baseline stays as the fast
fallback and the deopt target.

Target: close the compute gap on `mandelbrot` / `nbody` / `fib` (6–12× node →
~1.5–2×). Each stage is a multiplier, gated (test262 JIT-off == JIT-on
failing-set diff), and committed.

## Stages

1. **Type feedback (foundation, 0 perf by itself).** ✅ *landed.* The
   interpreter records observed operand representations per arithmetic /
   relational site during warm-up. See [`crates/otter-vm/src/jit_feedback.rs`]
   (`ArithFeedback`, an OR-accumulated `{Int32, Float64, String, BigInt, Other}`
   bitset) keyed on `Interpreter::jit_arith_feedback: (function_id, byte_pc)`.
   Recorded by the `run_*_regs` arithmetic helpers (gated on a JIT hook being
   installed, so interpreter-only runs pay nothing) and baked into
   `JitInstrView::arith_feedback` by `Interpreter::bake_arith_feedback` at
   tier-up. Call-site mono/poly feedback (`jit_call_site_feedback`) and method
   feedback (`jit_method_site_feedback`) already existed. Element-kind feedback
   is added when the IR first consumes arrays. *Verified: byte-identical
   test262 failing-set both JIT modes; no bench delta.*
2. **Typed SSA IR.** ⏳ *graph construction landed (int32 subset).* New
   `crates/otter-jit/src/optimizing/` (`pub mod optimizing`): `ir.rs` (typed
   SSA — `Graph` / `Block` / `Node` / `Repr{Tagged,Int32,Bool}`) and
   `builder.rs` (bytecode → SSA via Braun et al. sealed-blocks: CFG discovery,
   on-demand phi insertion, loop back-edges without dominance). Arithmetic /
   comparison sites read the stage-1 `arith_feedback` and lower int32-only
   sites to unboxed `Int32Add/Sub/Mul` / `Int32Compare` guarded by
   `CheckInt32`. Whole-function bail to baseline via `Unsupported` on any
   opcode / operand / branch / feedback outside the subset. `build_graph(view)`
   is the entry; emits **no code yet** and is not wired into tier-up, so VM
   behavior is unchanged. Unit-tested: straight-line, diamond merge (phi),
   counting loop (header phis), opcode + feedback bails. *Remaining for stage 2:
   the float subset — `Repr::Float64`, `CheckNumber`, `Float64*` nodes,
   `Int32ToFloat64` widening, `LoadNumber` constant threading into the view,
   and `Div`/`Rem` — to cover mandelbrot/nbody; then `Call` (feeds stage 5).*
3. **Lowering + unboxed + linear-scan regalloc.** Float64/Int32 live in CPU/FP
   registers across the loop, boxed only at tagged-use boundaries. Emit arm64
   (dynasm). Adds LICM (`arr.length`, loop invariants) and bounds-check
   elimination. ~2–3× on mandelbrot.
4. **Deopt.** A failed type guard reconstructs the baseline/interpreter frame
   from per-guard deopt metadata (SSA values → interpreter registers) and
   resumes there — making speculation safe.
5. **Speculative inlining** of monomorphic tiny callees (fib recursion,
   sort comparator, map/filter callback) — kills the per-call frame-build tax
   (~57% of fib self-time). ~2× on call-bound code.

GC contract carried over: only the frame register window is rooted; unboxed
numbers in registers across a safepoint are safe (not pointers), boxed pointers
in registers across a safepoint require a flush/stack-map. Deopt must
reconstruct exact interpreter state. Regex stays out of scope.

_Part II started 2026-06-20 on `perf/engine-rewrite`._
