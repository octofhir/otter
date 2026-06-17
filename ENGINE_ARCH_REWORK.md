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

## R2 EXECUTION PLAN — slots → shape (full attribute-encoding shapes)

Status: D1a-d landed (ObjectBody 424B → 160B, −62%). Remaining fat: `slots:
SmallVec<[SlotMeta;4]>` (72B) + `inline_values[6]` (48B). This is the plan to
remove `slots` (−72B → ~88B) by making the **shape** own per-slot attributes,
V8-style. High regression risk (touches all property/descriptor/accessor/
enumeration/freeze semantics) — every stage gated by a FULL test262 run with
failing-set diff vs the pre-stage baseline.

Current architecture (confirmed):
- `ShapeBody` (object/shape_body.rs) = `{id, parent, transition_key,
  property_count, own_offset}`. Each node = one append transition. NO attributes.
- `shape_runtime.rs` `child_with_roots` = transition cache keyed by **key only**
  → returns/creates the child shape for appending `key`.
- Per-object `slots: SmallVec<[SlotMeta;4]>`; `SlotMeta = {flags:
  PropertyFlags, kind: Data | Accessor(Box<AccessorPair>)}`. Sole holder of
  attributes + accessor getter/setter. Kept in lockstep with the value array
  (debug_assert in trace). 37 access sites (object.rs 32, descriptor_core 2,
  shape_transition 3).
- Value array: `inline_values[6]` + boxed `overflow_values` (in ExoticSlots).
  Accessor slots store `undefined` in the value array; getter/setter live in
  `SlotMeta.kind`.

Stages (each: implement → `just test262` full → diff failing-set → commit):
- **A. Shape carries attributes.** Add `own_flags: PropertyFlags` +
  `own_is_accessor: bool` to `ShapeBody::child`. Key the transition cache by
  `(key, flags, is_accessor)` so the same key with different attributes is a
  distinct transition (this is what invalidates ICs correctly). Add
  `shape_slot_attrs(shape, offset) -> (PropertyFlags, bool)` chain-walk reader
  (mirror `shape_offset_of_key`). Dual-write: keep `slots` authoritative; shape
  records redundantly. Assert shape-attrs == slots-attrs in debug. No behavior
  change, no size change — proves the machinery.
- **B. Flip reads to the shape.** Route `slot_lookup_at` / `slot_descriptor_at`
  / enumeration flag reads / `is_data` checks to read flags+kind from the shape
  for shaped objects (dict mode still uses slots). Accessor getter/setter still
  in `slots` for now.
- **C. Migrate accessor storage to the value slot.** A shaped accessor slot
  stores an `AccessorPair` GC cell handle in the value array; shape's
  `own_is_accessor` marks it. Remove `kind` from per-object storage.
- **D. defineProperty / freeze / seal as attribute transitions.** Changing
  w/e/c or data↔accessor transitions the object to the attribute-encoding
  shape instead of mutating `slots[i].flags`. Freeze/seal → bulk transition.
- **E. Remove `slots` from shaped objects.** `slots` becomes dict-mode-only;
  move it into `ExoticSlots` (like `dictionary_keys`). Shaped objects (the
  common case) carry no `slots`. **ObjectBody → ~88B.**

Gotchas: transition-cache blow-up if attrs over-specialize (mitigate: only
non-default attrs create attr-transitions; default w+e+c data is the fast
spine). Symbol-keyed props keep `SlotData` in `ExoticSlots.symbol_props`
(unaffected). The JIT WhiskerIC already guards on shape id, so attr-transitions
auto-invalidate it.

Best executed as a dedicated session per stage — not interleaved with other
work — because of the conformance-gating cadence.

### Critical findings (scoped before implementation)
- **Shaped objects are NOT always all-default-data.** `o.x = v` appends a
  default-data slot and keeps the shape. But `Object.defineProperty` on an
  *existing* property modifies `slots[i]` in place via `set_slot`
  **without nulling the shape** (object.rs:2384 / 2428 / 2516), and
  `freeze`/`seal` flip `configurable`/`writable` on `slots` in place
  (object.rs:2800-2829) — both on shaped objects. So a shaped object CAN carry
  non-default flags / accessor kind. ⇒ the trivial "lazy-slots = derive
  default-data from shape count" shortcut is UNSOUND as-is; those mutation
  paths must first transition to an attribute-encoding shape (stage D) or
  materialize a per-object slots override.
- **`slots.len()` is the load-bearing property COUNT source** during
  transitions (`push_slot` uses `self.slots.len()` as the next slot index;
  shape_transition replays against it). Removing `slots` requires the shape
  (or dict keys) to become the authoritative count everywhere first — this is
  entangled with the transition replay machinery, not a local edit.
- Net: R2 is a genuine multi-file rewrite of the shape/transition core, not a
  mechanical field move like D1a-d. Execute stages A→E fresh, each with a full
  `just test262` failing-set diff. Lazy-slots can be folded in as an
  optimization within stage E (a shaped object with only default-data slots
  stores no override) once the shape owns count + attributes.

## R2b (later) — Split inline[6] + overflow `Vec` object storage → the 7th property cliff

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

---

# Deep tier — the structural ceilings under the cliffs

R1–R4 are *cliffs* (common cases falling off a fast path). The levers below are
*ceilings*: even when everything works, these caps the engine at multiples
of node. Each is a breaking redesign. Reproducers in `benchmarks/micro/`.

## D1 — `ObjectBody` is a ~519-byte god-struct (the allocation ceiling)

### Evidence
```
benchmarks/micro/alloc.js   3M × {a:i,b:i+1}   →  otter 1733 ms / 1557 MB / 24 GC cycles
                                                   node  ~130 ms compute
```
**519 bytes allocated per two-field object** (1557 MB ÷ 3M), **~578 ns per
allocation** vs node ~40 ns → **~13×**. `propStubs=2`: the `a`/`b` reads inline
fine; the entire cost is *allocation + GC of bloated objects*.

### Root cause
`ObjectBody` (`object.rs:389`) carries ~25 fields **inline in every ordinary
object**, almost all for rare/exotic cases that 99% of objects never use:
`inline_values:[Value;6]` (48 B), `overflow_values: Vec`, `dictionary_keys:
Vec<String>`, `dictionary_index: FxHashMap<String,u16>` (~48 B), `slots:
SmallVec<[SlotMeta;4]>`, `symbol_props: Vec`, `host_data: Option<Box<dyn Any>>`,
`call_native`, `constructor_native`, and `boolean_data` / `number_data` /
`string_data` / `symbol_data` / `bigint_data` / `date_data` / `error_data` /
`is_raw_json` / `is_arguments_object` / `extensible` … A `{a,b}` literal
allocates and zero-inits all of it.

### Breaking redesign
**Split the god-struct.** Minimal hot core: `{ shape, proto/jit_proto,
flags, values[] }` (the flat slab from R2). Move every wrapper/exotic slot
(Date, Error, Boolean/Number/String/Symbol/BigInt data, arguments map, host
data, native call/construct, dictionary mode) behind a single lazily-allocated
`Option<Box<ExoticData>>` or separate body type tags. Target ~48–80 B for a
plain object (≈7–10× smaller). Direct effects: less malloc, far fewer GC
cycles (json's 387 MB / 10 cycles collapses), better cache density on every
property access, faster zero-init. This is the **single biggest allocation
lever** and underpins json (6.5×), prop-access (8.6×), array-ops (7.9×), and
every OO workload.

## D2 — No inline allocation: `new` / object & array literals bridge to the interpreter

### Evidence
`alloc.js` above: 578 ns/object even though `{a,b}` needs no overflow, no
exotic slots. `Op::NewObject` / `Op::NewArray` fall through the generic
`jit_delegate_op_stub` (`baseline.rs:1728-1733`) into the full interpreter
allocation path (shape resolve + GC alloc + init) per object.

### Breaking redesign
Emit an **inline bump-allocation fast path** in compiled code for
fixed-shape object/array literals and monomorphic `new`: reserve N bytes from
the nursery bump cursor (compare against limit, branch to a slow stub only on
nursery-full), stamp the header + known shape, store the field values inline —
no VM round-trip. The nursery already *is* a bump allocator
(`heap.rs:129`, `compressed.rs:409`); expose its cursor/limit to emitted code.
Pairs with D1 (small fixed body = easy inline alloc).

## D3 — Variable-size payloads are malloc'd `Vec`s, not GC-inline storage

### Evidence
json self-time: **36.6% `libsystem_malloc` + 6.4% memcpy** (audit §2). The GC
*cells* bump-allocate, but a string's code units (`Vec<u16>`/`Vec<u8>`,
`string/gc_body.rs`), an array's elements (`ArrayBody` Vec), and an object's
overflow values (`object.rs:403`) are **separate malloc'd `Vec`s** hanging off
the cell — one malloc/free per string/array, plus a pointer-chase per access,
plus special-cased GC tracing of malloc-owned slots (`barrier.rs:31`).

### Breaking redesign
**Variable-size GC cells with inline trailing storage** (bump-allocated,
movable): store small string bytes / array elements / overflow values in a
flexible-array tail of the cell instead of a side `Vec`. Removes the malloc/free
tax (json's 43%), the indirection, and the malloc-owned-slot barrier special
case. Large GC change but it is the other half (with D1) of "stop calling
malloc on the hot path."

## D4 — `Math.sqrt`/`sin`/`cos` bridge to libm instead of a native instruction

### Evidence
```
benchmarks/micro/sqrt.js   10M Math.sqrt   →  otter 1748 ms   node ~5 ms compute  (~100×+)
```
~175 ns per `Math.sqrt` call. It is dispatched as a normal method/global call →
native function → `libsystem_m` (libm). nbody and mandelbrot lean on `sqrt`.

### Breaking redesign
Recognize `Math.{sqrt,abs,floor,ceil,round,min,max,…}` as **intrinsics** at
compile time and emit the native instruction inline (`fsqrt`, `fabs`,
`frintm`, …) under a "Math is the original global" guard — no call, no libm.
Cheap, isolated, and directly lifts the float benches.

## D5 — Codegen is 100% memory-bound: no CPU register allocation

### Evidence
mandelbrot is **74.9% in its own compiled loop** yet still ~7× node on compute
(audit §2). Every operand is `ldr x9,[x19,off]` and every result `str
x9,[x19,off]` (`baseline.rs:1322/1404/…`) — the baseline JIT, by design (*"no
register allocation"*), round-trips **every value through the in-memory frame
window every op**. mandelbrot's `x,y` are reloaded from memory on each use; node
keeps them in FP registers across the loop body.

### Breaking redesign
This is the core of the **optimizing tier**: a real (even linear-scan)
register allocator that keeps loop-body live values in CPU/FP registers and
spills only at boundaries, plus unboxed `f64`/`i32` SSA values (box only at tier
edges), LICM, and bounds/guard-check elimination. This is the umbrella lever
from the audit; D1–D4 are the down-payments that shrink what it must cover.

---

# Recommended sequencing (breaking, bankable in order)

Ordered by gain ÷ effort, dependencies respected. Each row is independently
measurable against a committed reproducer.

| step | lever | scope | expected | unlocks |
|---|---|---|---|---|
| 1 | **R1** self-priming property IC | JIT stub + IC fill | 5–9× on top-level code | de-risks all IC work |
| 2 | **VmError shrink** (audit A) | mechanical, Box payloads | 5–15% broad | cleaner profiles |
| 3 | **D4** Math.* intrinsics → native `fsqrt`/… | compiler + emit | float benches | cheap, isolated |
| 4 | **R3** inline array `.length` | emit + array header | kills per-iter length stub | every array loop |
| 5 | **D1+R2** object-model rewrite: split god-struct + flat slab | ObjectBody, GC tracing, body offsets | ~13× alloc, ≈7–10× smaller objects, fewer GC cycles | json/prop/array/OO |
| 6 | **D2** inline bump-alloc in JIT | emit + nursery cursor | per-`new` floor gone | pairs with D1 |
| 7 | **D3** variable-size GC cells (inline string/array storage) | GC + string/array bodies | kills json's 43% malloc | alloc-heavy |
| 8 | **R4** register-window calling conv + builtin-callback inlining | call path | 2–4× call-bound (fib/sort/array) | — |
| 9 | **D5 / optimizing tier** — regalloc + unboxed SSA + speculative inline + LICM + bounds-elim + deopt | new tier | 2–5× numeric/OO, the residual ceiling | umbrella |

**Strategy.** Steps 1–4 are each a few-hundred-line, contained, independently
measurable change — the high-ROI down-payment. Steps 5–7 are the **object &
memory rework** (the biggest single win for allocation-bound code: json,
prop-access, array-ops) and the place where breaking layout/GC freedom pays off
most. Steps 8–9 are the codegen ceiling. The optimizing tier (9) is last by
design: R1–D3 remove the cliffs and most of the allocation tax first, so the
optimizing tier has a much smaller surface to cover and a clean base to
speculate on. Regex stays out until the core lands.

_Generated 2026-06-17. Reproducers: `benchmarks/micro/`. Profiles: `benchmarks/profiles/`._
