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
