# JIT top-level compute-loop slowness — root cause + plan

Session findings. Tree is clean: method-call IC committed (`dff6c916`); all
optimizing-tier experiments reverted (had a miscompile, see below). Start fresh
from here.

## What shipped this session (committed, gated, correct)

`dff6c916` perf(vm): cache dense-array method dispatch at the call site.
- Monomorphic method-call IC keyed by call-site id for native `Array.prototype.*`
  builtins. Records resolved `%Array.prototype%` shape + slot + an
  `ArrayMethodTag`; warmed site validates by shape+slot-identity (no key hash,
  builtin confirmed by stable native fn pointer — no cached GC value) and
  dispatches the tag through `array_live_method_dispatch`, now an enum jump table
  instead of a 26-arm string match. A live IC short-circuits the redundant
  `jit_prepare_direct_method_call` double-resolution and self-heals when the
  receiver stops being an array.
- **tree-traversal 293→262ms (-10.6%)**. Parity 10/10, test262 byte-identical
  (Array/Object 0, expressions 17, statements 5), GC_STRESS=128 clean.

## THE BIG FINDING: top-level compute loops run 100% interpreted

`prop-access` (5x vs bun), and any top-level loop with a numeric accumulator, get
**0% JIT** — fully interpreted. Confirmed via profiling: `maybe_dispatch_jit` 0%,
`enter_compiled` 0%, `dispatch_loop_inner` dominates.

NOT the cause (ruled out): globals, top-level await / async frame, method bridges.
- `tree`/`fib` JIT fine because their hot work is inside FUNCTIONS (locals →
  registers, function-entry tier-up). `prop-access` hot work is INLINE in the
  top-level loop → relies on top-level loop OSR.

**Root cause = int32-overflow deopt permanently disables the loop.**
- The optimizing tier (Cranelift) speculates unboxed int32 from warmup feedback.
- An accumulator that starts int32 and grows past 2^31 (`acc += ...`) overflows.
- `Int32Add`/`Sub`/`Mul` deopt-on-overflow, resuming the interpreter at the arith
  byte-PC. `maybe_osr` sees `Bailed(pc)`, the bail is inside the target loop →
  `jit_osr_disabled.insert((fid, osr_pc))` → header disabled FOREVER → interp.
- Operand feedback stays int32 (operands ARE int32; only the *result* overflows),
  so a naive recompile deopts again — hence the permanent disable.

Proof:
- v5b `for(i<3M) acc=(acc+i*7)|0` → **932ms** (interp), bails at the `Add`.
- v5c same but `acc=(acc+i*7)&255` (no overflow) → **16ms** (JIT). 
- v7 `for(i<10M) acc=acc+i` (overflows, no `|0`) → 74ms once widening added (below).

Repro files were in scratchpad; recreate easily. Profiling recipe: `dsymutil`,
`samply record --save-only`, `/tmp/prof2.py` (self) `/tmp/prof3.py` (inclusive),
`OTTER_JIT_TRACE=1` prints `[otter-jit] optimizing tier compiled/declined fid N`.

## The correct fix (Maglev-style deopt-and-reoptimize) — designed, partly built, NOT shipped

Three pieces. (1)+(2) were built and **verified correct** (v7 matches node:
`49999995000000`). (3) had a **miscompile** → everything reverted to stay correct.

### (1) Representation-widening reoptimization  — CORRECT, reverted with the rest
On an int32-overflow bail whose resume PC is an `Add/Sub/Mul`, record
`(fid, byte_pc)` in a `jit_arith_widen_float` set, drop all cached compilations of
`fid` (`jit_code`, `jit_code_cache`, `jit_osr_code`, `jit_osr_disabled`), and
RE-ARM instead of disabling. `bake_arith_feedback` forces those sites'
feedback to `ARITH_INT32|ARITH_FLOAT64` (numeric, not int32-only) → builder
lowers `Float64Add` (operands widened via `Int32ToFloat64`) → no overflow deopt.
Widen each site once (second bail at same PC = genuine miss → normal disable).
Wire into both `maybe_osr` and `maybe_dispatch_jit` bail arms. Detect the op via
`context.exec_function(fid).instr_at_byte_pc(pc).op()`.

### (2) Float operand forces the float path — CORRECT, reverted with the rest
Once an accumulator widens to `Float64`, downstream int32-speculated ops hit
`CheckInt32(Float64)` → `Unlowered("check-int32 operand not tagged/int32")` →
decline. Fix: in `arith_binop`, `compare`, `arith_node_binop` — if either operand
node's repr is already `Float64`, take the float path regardless of int32
feedback (the speculation was simply wrong; JS arithmetic on a float is float).
`float_operand`/`float_node_operand` already widen int32 operands. Add a tiny
`operand_is_float64(block, reg)` helper.

### (3) `(float)|0` bitwise truncation — MISCOMPILE, do this carefully
Bitwise / shift ops coerce operands via JS `ToInt32` regardless of type. Once an
operand is `Float64`, `int32_operand`/`int32_node_operand` must truncate it, not
`CheckInt32`-deopt. Also `bitwise_binop` rejects non-int32 feedback too early
(must accept numeric/float; let the operand resolver coerce). Needs a new
`NodeKind::Float64ToInt32(NodeId)` (repr Int32) lowered in BOTH optimizing
backends (Cranelift `clif/lower.rs` tried first; `emit.rs` dynasm fallback used
when the function has method calls — clif has no runtime-call support).

**THE BUG that forced the revert:** my lowerings produced wrong results.
`ToInt32(2500000.0)` returned 500 (clif) — off by orders of magnitude, so almost
certainly a value-routing bug (likely the `f64` operand reaching the lowering as
its BOXED i64 bits rather than as an FP value — check how `self.val(operand)` /
the FP home is obtained vs how `Float64Add` reads its operands; `Float64Add` uses
`load_fp_loc`). For `emit.rs`, arm64 has **`fjcvtzs`** (ARMv8.3 "JavaScript
convert to signed") — dynasm-rs supports it (`opmap` `[W, D] => [R(0), R(5)]`),
it computes ECMAScript `ToInt32` in one instruction (Apple Silicon has it) — but
verify the operand actually arrives in an FP home and the result store/repr is
right. The clif inline algorithm I wrote (trunc → mod 2^32 → signed-wrap, NaN/∞→0)
is mathematically correct; the bug is in how operands/results are plumbed, not the
math.

### Verification done
- (1)+(2): v7/v9 — `acc` correct (`2500000`, `49999995000000`), matches node.
- prop-access with (3) disabled (`OTTER_NO_TOINT32`) = `512493014` CORRECT (just
  declines to interp on the bitwise-float site). With (3) on → wrong / "value is
  not a function". So (1)+(2) are sound; only (3)'s lowering is broken.

### Build/gate recipe (run after every change)
```
cargo build --release -p otter-cli            # binary = target/release/otter
cargo test -p otter-vm -p otter-jit -p otter-compiler
for b in fib tree-traversal mandelbrot nbody sort array-ops string-ops json prop-access regex; do
  diff <(OTTER_JIT=0 target/release/otter run benchmarks/scripts/$b.js) \
       <(target/release/otter run benchmarks/scripts/$b.js) && echo "$b OK"; done
OTTER_GC_STRESS=128 target/release/otter run benchmarks/scripts/tree-traversal.js
# test262 failing-set must be byte-identical: language/expressions=17,
# built-ins/Array=0, built-ins/Object=0, language/statements=5
```

## Suggested order for the fresh session
1. Re-land (1) widening + (2) float-forcing (they were correct). Gate fully.
   This alone fixes float-accumulator loops (v7-class) — correct, general.
2. Then add (3) `Float64ToInt32` carefully, debugging the operand plumbing with a
   tiny pure-clif `(floatval)|0` loop until `ToInt32(2500000.0)==2500000`, THEN
   the `emit.rs` `fjcvtzs` path (prop-access needs emit.rs — it has method calls).
   Verify prop-access output == `512493014` AND speeds up.
3. Other top-of-loop blockers seen in prop-access: `Rem` (`%`) unsupported in a
   loop (setup loop `i%1000`) → declines; consider supporting it.

No bridges-as-shortcut; inline guards in machine code. No env flags in shipped
code (the `OTTER_*` toggles above were diagnostic only and are reverted).
