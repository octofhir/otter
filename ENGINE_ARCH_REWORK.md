# Otter engine — aggressive architecture rework plan

Follow-up to [`ENGINE_PERF_AUDIT.md`](./ENGINE_PERF_AUDIT.md). The audit ranked
*where* time goes; this document goes one level deeper — to the **specific
data-structure and tiering decisions** that cause the gaps — and proposes
**breaking** redesigns. Backward compatibility is explicitly not a constraint:
object layout, bytecode, GC layout, calling convention, and tiering are all in
scope. Regex is deliberately **out of scope / last** (own engine, isolated
crate — fix after the core).

Every claim below is backed by a committed reproducer under `benchmarks/micro/`
and `OTTER_STATS=1` counters. Profiles in `benchmarks/profiles/`.

---

## The through-line

The baseline JIT removes interpreter dispatch, but **every value access still
goes through narrow, parasitic fast paths that fall off a cliff in extremely
common cases**, dumping the work into a runtime bridge stub that hashes the
property key. Four cliffs, each independently measurable, each fixable by a
breaking redesign:

| # | Cliff | Trigger (how common) | Measured penalty |
|---|---|---|---|
| R1 | Inline property IC never fills for **module top-level / default-OSR** code | every top-level hot loop | **5–9×** (1517 ms vs 168–281 ms; 36M stubs vs ~0) |
| R2 | Inline slots capped at **6**; 7th+ own property spills to a `Vec` and never inline-caches | any object with >6 fields | **+43%** + 1 stub per access (cap7 673 ms vs cap6 471 ms) |
| R3 | Array **`.length`** read goes through the property stub; **no LICM** hoists it | every `for(i<arr.length)` loop | **1.7×** (471 ms vs 279 ms; 10M stubs vs 0) |
| R4 | Per-call **frame build** dominates compiled calls | every non-inlined call | **fib 56%** of self-time in call setup |

Plus the cross-cutting **VmError fat-enum drop** (5–15% everywhere, audit §4)
and the umbrella **no-optimizing-tier** box/unbox tax (mandelbrot's 75%-compiled
loop still ~7× node on compute).

---

## R1 — Inline property IC is parasitic on interpreter warming → top-level code never inlines

### Evidence
```
benchmarks/micro/cap6.js          loop INSIDE a function   →  ~0 object-prop stubs, fast
benchmarks/micro/toplevel_cliff.js SAME loop at top level   →  36,001,002 stubs, ~5-9× slower
```
```
$ OTTER_STATS=1 otter run toplevel_cliff.js                 # jitRuntimePropertyStubs = 36,001,002  (~1517 ms)
$ OTTER_JIT_OSR_THRESHOLD=1     otter run toplevel_cliff.js  # stubs = 8  (~170 ms)   ← inlines
$ OTTER_JIT_OSR_THRESHOLD=100000 otter run toplevel_cliff.js # stubs = 8  (~274 ms)   ← inlines
```
The default OSR threshold (`JIT_OSR_THRESHOLD = 1000`, `lib.rs:1302`) is the
**pathological** point: both a lower and a higher threshold inline. A loop
OSR'd inside a once-called function (`benchmarks/micro/` posr pattern) also
inlines. Only module-top-level + default-threshold misses. `jitCompileAttempts=1`
in all cases — this is *not* a recompile bug.

### Root cause
The JIT's `WhiskerIcCell` (`jit/baseline.rs:355`) is filled by
`jit_load_prop_stub` **only when the interpreter already warmed
`load_property_ics[site]`** to a single monomorphic `OwnData` entry
(`property_dispatch.rs:3009-3025`, `whisker_load_cell_fill:3045`). The stub
**reads** a pre-warmed IC; it does not **install** one itself. So the inline
cache is parasitic on how much the interpreter ran the site before tier-up —
which depends on OSR timing in a way that has a dead zone at the default
threshold.

### Breaking redesign
**Make the JIT property stub self-priming.** On a monomorphic own-data hit, the
stub should install/update the IC entry *and* fill the WhiskerIC cell directly,
independent of any prior interpreter warming. Concretely:
- `jit_runtime_load_property`/`store_property`: on a successful own-data read,
  always compute the `(shape, slot)` and return the cell fill, even when
  `load_property_ics[site]` was empty — and populate that entry too.
- Decouple cell-fill eligibility from `entry_count()==1` pre-warming; base it on
  the *observed* receiver this call plus a one-slot shape check next call (the
  inline guard already re-checks the shape, so a wrong fill simply misses).
- Audit the OSR-compile path to confirm module-top-level functions emit the
  same WhiskerIC sites as nested functions (they get IC sites — `executable.rs:338`
  — so the gap is fill, not emit).

Impact: lifts **every top-level hot loop** (and any code that tiers up before
the interpreter warms it) onto the inline path. 5–9× on the affected shape of
code, which includes most real scripts and several benchmarks here.

---

## R2 — Split inline[6] + overflow `Vec` object storage → the 7th property cliff

### Evidence
```
benchmarks/micro/cap6.js  (6 fields)  →  471 ms,  object-prop stubs ~0
benchmarks/micro/cap7.js  (7 fields)  →  673 ms (+43%),  +10,000,000 stubs (one per `g` access)
```
nbody confirms in the wild: `Body{x,y,z,vx,vy,vz,mass}` — `mass` is slot 6,
out-of-line; advance()/energy() read `bj.mass` in the inner loop → **1.4M
load-property stub hits + `eq_str` per access** (audit §2 nbody).

### Root cause
`ObjectBody` (`object.rs:389`) stores the first `INLINE_VALUE_CAP = 6`
(`object.rs:183`) own-data values inline; the rest spill to a separate
`overflow_values: Vec<Value>` (`object.rs:403`) — a second heap allocation
reached by pointer-chase. `whisker_load_cell_fill` refuses any slot
`>= INLINE_VALUE_CAP` (`property_dispatch.rs:3053`) because the inline machine
code only knows how to load at `inline_values_offset + slot*8` from the object
pointer — it cannot express "deref the overflow Vec, then index." So **every
own property past the 6th is permanently on the stub + key hash + double
indirection.**

### Breaking redesign
**Uniform, shape-sized, contiguous own-data value array.** Replace the
`inline_values: [Value; 6]` + `overflow_values: Vec<Value>` split with a single
flat slab whose length is the shape's field count, allocated inline with the
object body (variable-size GC allocation) or as one right-sized block. Then:
- `value_byte(slot) = header + values_offset + slot*8` for **any** slot — the
  WhiskerIC inline load works unchanged for slot 6, 60, or 600.
- One allocation, one cache line stride, no Vec pointer-chase.
- Shape transitions still append; growing the slab on transition is the same
  cost class as growing the Vec, but the steady-state read is a single load.

This is the "flat object layout / StoneMap" direction. It removes R2 entirely
and makes R1's inline path apply universally. Combined with R1, **all
monomorphic own-property access becomes shape-guard + one load**, no hash, no
stub, for objects of any size.

---

## R3 — Array `.length` stubs every iteration; no LICM

### Evidence
```
for(i<arr.length) …   →  471 ms, 10,000,000 stubs   (benchmarks/micro/cap6.js)
const n=arr.length; for(i<n) …  →  279 ms (1.7×), ~0 stubs   (cap_length_hoisted.js)
```

### Root cause
`arr.length` is an `Op::LoadProperty` with a string key `"length"`. On an array
receiver it is not an own-data inline slot, so the WhiskerIC never fills and it
takes the bridge stub (`jit_runtime_load_property`) **every iteration**. Two
missing mechanisms: (a) no inline fast path that reads the array length directly
from the array header in compiled code; (b) no **LICM** to hoist the
loop-invariant load out of the loop (node does both).

### Breaking redesign
1. **Inline `.length` for array/typed-array receivers**: emit a tag + body-type
   guard then a direct header-field load of the length, no stub, no key. Same
   shape as the property WhiskerIC but keyed on "receiver is array" rather than a
   shape slot.
2. **LICM in the (future) optimizing tier**: hoist provably loop-invariant
   loads (`arr.length` where `arr` is not reassigned, invariant field reads).
   Until the optimizing tier exists, (1) alone removes the 10M stubs.

Pervasive: essentially every array-iterating loop pays this today.

---

## R4 — Per-call frame build dominates compiled calls

### Evidence
fib self-time (`benchmarks/profiles/fib.on.selftime.txt`): ~56% across
`prepare_jit_direct_call_frame` (18.6%), `jit_prepare_direct_call` (14.6%),
`jit_finish_direct_call_returned` (8.5%), `draw_registers` (4.6%),
`build_upvalues_for_count` (2.5%), `bytecode_call_target_parts` (2.3%) — on the
*optimized* compiled→compiled direct-call path. sort/array-ops callbacks pay the
same per element (`run_bytecode_callable_committed` + `bind_bytecode_call_arguments`).

### Root cause
Each call builds a fresh frame, binds arguments into a new register window,
draws/reclaims that window, and builds upvalues — per call, even when the callee
is monomorphic and tiny.

### Breaking redesign
- **Register-window calling convention**: lay caller and callee register windows
  contiguously so arguments are passed in place; skip `bind`/`draw`/`reclaim` on
  the monomorphic fast path.
- **No-upvalue fast path**: skip `build_upvalues_for_count` for callees that
  capture nothing (the common case).
- **Inline builtin callbacks**: splice a monomorphic JS callback body into
  `Array.map/filter/forEach/reduce/sort`'s native iteration so there is no
  per-element re-entry (kills the sort 2.4× / array-ops 2.7× ceiling).

Partly subsumed by the optimizing tier's inlining, but the calling-convention
work stands alone and is cheaper.

---

## Recommended sequencing (breaking, bankable in order)

1. **R1 self-priming property IC** — smallest change, 5–9× on top-level code,
   de-risks everything else. *Contained to the JIT stub + IC fill.*
2. **VmError shrink** (audit Lever A) — mechanical, 5–15% broad.
3. **R2 flat object storage** — removes the 6-field cliff; makes R1 universal.
   *Breaks ObjectBody layout, GC tracing, every body offset constant.*
4. **R3 inline `.length`** — removes the per-iteration array-length stub.
5. **R4 calling convention + builtin-callback inlining** — call-bound benches.
6. **Optimizing tier** (audit Lever B) — the umbrella: unboxed type-specialized
   SSA + speculative inline + LICM + bounds-elim + deopt. Subsumes the residual
   box/unbox tax and the LICM half of R3. Large, multi-month; R1–R5 are the
   high-ROI down-payment that makes its job smaller.

R1–R4 are each a few-hundred-line, well-scoped, **independently measurable**
change with a committed reproducer — not "rewrite the engine." They target the
cliffs that the optimizing tier would otherwise have to paper over.

_Generated 2026-06-17. Reproducers: `benchmarks/micro/`. Profiles: `benchmarks/profiles/`._
