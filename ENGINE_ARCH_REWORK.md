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

Plus the umbrella **no-optimizing-tier** box/unbox tax (mandelbrot's
75%-compiled loop still ~7× node on compute). The cross-cutting
**VmError fat-enum drop** from the audit has landed: user-facing string
payloads now live behind boxed `str`s, keeping `VmError` size-guarded at 24B
while preserving structured error classes.

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

### Landed redesign
**JIT property stubs are self-priming.** A compiled `LoadProperty` bridge now
probes any installed PIC entries, records miss accounting, and when the observed
receiver/key pair is cacheable, installs a `LoadPropertyIc` directly from the
compiled stub. If the new entry is monomorphic own-data, the stub returns the
packed WhiskerIC cell fill immediately, so the next compiled iteration runs the
inline shape/value-slab load even when the interpreter never warmed that site.
Direct-prototype data installs too, but still returns no Whisker fill because
the current inline load cell only models own data slots.

`StoreProperty` mirrors this for existing own writable data slots: the compiled
stub installs and replays an existing-own-data store candidate directly, then
returns the Whisker store fill when the site is monomorphic. Shape-growing store
transitions still go through the ordinary slow path because transition capture
mutates the object shape.

Impact: top-level/default-OSR code no longer depends on interpreter pre-warming
to leave the property bridge stub. The OSR path already emitted WhiskerIC sites;
the missing piece was runtime fill.

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
- **A. Shape carries attributes. — LANDED.** Added `own_flags: PropertyFlags`
  + `own_is_accessor: bool` to `ShapeBody`; transition cache now keyed by
  `(parent, key, flags, is_accessor)` so the same key with different attributes
  is a distinct transition (correct IC invalidation). `shape_slot_attrs(shape,
  offset) -> (PropertyFlags, bool)` chain-walk reader added. Dual-write: `slots`
  stays authoritative; shape records redundantly. Debug assert
  (`debug_assert_appended_shape_slot`) checks the freshly appended slot matches
  the shape — scoped to the append, not the whole object, because
  defineProperty/freeze/seal still mutate slot flags in place until stage D, so
  older slots may legitimately diverge. Attr accessors are
  `#[cfg_attr(not(debug_assertions), allow(dead_code))]` until stage B reads
  them in release. No behavior change, no `ObjectBody` size change. Gate: full
  test262 (Atomics excluded — timeout-grind, orthogonal) failing-set diff vs
  HEAD = **0 regressions** (52783 tests, 1373 fail; ±2 staging/sm flake only).
- **B. Flip reads to the shape. — LANDED (partial, spec-visible readers).**
  Added a per-object `slot_attrs_overridden` bit (rides in the padding beside
  `extensible` — `ObjectBody` stays 160B, asserted). The bit is set when an
  in-place attribute mutation (`defineProperty` on an existing slot via
  `set_slot`, `seal`, `freeze`) changes a shaped slot without transitioning the
  class. New `ObjectBody::slot_attrs(heap, i)` reads `(flags, is_accessor)` from
  the shape for a shaped, non-overridden object and falls back to `slots`
  otherwise; the three spec-visible readers (`slot_lookup_at`,
  `slot_descriptor_at`, `slot_data`) route through it. Accessor getter/setter
  pair still read from `slots` (migrates in C). Behavior-identical: for
  non-overridden objects shape attrs == slots (stage A invariant), for
  overridden it's the old slots path. Gate: full test262 vs stage A = **0
  regressions** (±2 staging/sm flake, confirmed identical in isolation on both).
  REMAINING for B: the scattered mutation-internal `.kind.is_data()` /
  `.flags.*` reads (is_sealed, integrity loops, enumeration) still read `slots`
  — correct (slots authoritative), to flip before E can drop `slots`.
- **C. Migrate accessor storage to the value slot. — LANDED.** String-keyed
  accessor slots now store a GC-managed accessor cell in the flat value slab;
  per-slot metadata carries only flags plus the accessor discriminator.
- **D. defineProperty / freeze / seal as attribute transitions. — LANDED.**
  Existing shaped-string redefinitions and integrity operations rebuild the
  attribute-encoding hidden class instead of mutating shaped slot flags in
  place; dictionary/no-shape fallbacks still materialize metadata.
- **E. Remove `slots` from shaped objects. — LANDED.** Per-slot metadata moved
  into `ExoticSlots` and is allocated only for dictionary-mode or
  attribute-overridden objects. Shaped ordinary objects derive count and
  attributes from the shape and carry no slot metadata. **ObjectBody is now
  size-guarded at 72B.**

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

### Landed redesign
**Uniform contiguous own-data value slab.** The
`inline_values: [Value; 6]` + boxed `overflow_values: Vec<Value>` split is gone.
`ObjectBody` now owns one flat string-keyed value slab plus a cached slab base
pointer at a fixed `#[repr(C)]` offset:
- The IC cell encodes `slot * size_of::<Value>()`, with no inline-cap check.
- Emitted WhiskerIC code guards the receiver shape, reads the current slab
  pointer from the object body, and loads/stores `slab_base + slot_byte`.
- Shape transitions still append to the same logical slab, so slots 6, 60, and
  600 are IC-eligible instead of permanently falling back to the property stub.

This keeps object identity stable on the current fixed-size GC cell model while
removing the 7th-property cliff from the inline-cache policy. A future
variable-size GC cell pass can fold the slab into the object allocation itself;
the visible object/value-slot contract is already flat.

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

### Landed redesign
**Inline ordinary Array `.length` in the baseline JIT.** `LoadProperty` sites
whose constant key is literal `"length"` carry a compile-time metadata bit in
the VM→JIT snapshot. The arm64 emitter tries a receiver tag + `ArrayBody` type
tag guard before the normal property IC path; on a hit it reads
`ArrayBody.length` directly and boxes it as an int32. Lengths outside the int32
range miss back to the existing runtime property path, which preserves the
general numeric semantics.

This removes the array-length bridge-stub tax for ordinary hot loops while
leaving prototype/sparse/accessor semantics owned by the existing runtime
fallback. Typed-array `.length` and real LICM remain future work: typed arrays
need their own receiver kind guard, and loop-invariant hoisting belongs in an
optimizing tier rather than the current linear baseline emitter.

Pervasive: before this change, essentially every array-iterating loop paid this.

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
- **No-upvalue fast path — LANDED (direct-call frame setup).** Compiled→compiled
  direct calls now bypass the generic upvalue builder when the callee has no own
  captures, carrying the already-resolved parent spine straight into the callee
  frame. Capturing callees keep the old allocation path.
- **Borrowed lean callback arguments — LANDED.** Array iteration, Array sort,
  Map.forEach, and Set.forEach already reuse one reservation-stable stack for
  bytecode callbacks; the lean path now binds fixed borrowed argument arrays
  directly into the callee frame instead of materializing a per-element
  `SmallVec`. Generic/native fallback paths still own their argument vectors.
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

### Current status
Plain object bodies are split: dictionary keys/index, materialized slot
metadata, symbol props, host data, native call/construct hooks, wrapper payloads,
Date/Error/raw-JSON/arguments markers, and non-ordinary prototypes live in one
lazy `ExoticSlots` box. The hot `ObjectBody` is size-guarded at **72B** and
contains the JIT-visible shape, value-slab pointer/vector, prototype mirror,
extensibility/override bits, and optional sidecar pointer.

Array bodies now follow the same pattern for cold array-exotic state:
`ArrayBody` keeps only dense `elements`, logical `length`, and one optional
`ArrayExoticSlots` sidecar. Sparse/named/accessor/symbol properties,
descriptor flags, captured JSON source bytes, dirty state, extensibility, and
per-instance prototype overrides moved out of the plain dense array body. The
JIT-visible `elements`/`length` offsets remain baked from `offset_of!`, and the
hot array shell is size-guarded at **<=48B**.

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

### Current status
A first safe JIT allocation slice landed after the `MathCall` work:
`Op::NewObject` now uses a dedicated compiled-code bridge into the VM's
stack-rooted object allocator instead of the generic opcode delegate. This
keeps moving young-GC semantics identical to the interpreter path and removes
the decode/dispatch envelope around object allocation, but it intentionally does
not expose raw nursery cursor/limit state to machine code yet.

Measured on `benchmarks/micro/alloc.js` with `OTTER_STATS=1` against clean
pre-change commit `e131c2e8`: **1691 ms → 1722 ms** (noise/regression within
the same allocation/GC envelope), with unchanged `gcAllocBytesTotal`
`576425280` and `gcCycles=22`. The remaining D2 work is the actual inline
nursery allocation path; this bridge slice mainly prepares a narrower allocation
stub boundary.

The next structural cleanup removes a hidden per-object allocation cost from
the fast shaped path: shaped objects now leave dictionary identity unassigned
and read identity from the installed shape, while dictionary/attribute-overridden
objects lazily receive a fresh fallback id before losing their shape. That keeps
observable shape identity stable for dictionary mode and makes fixed-shape
object bodies templateable for the raw bump-allocation path.

The allocation boundary now has a real no-GC young-allocation primitive:
`GcHeap::try_alloc_no_collect` materializes a typed nursery cell only when cap
accounting, GC-stress, growth-ratio major GC, large-object handling, and nursery
space all allow a safepoint-free allocation. `NewObject` uses it for shaped
ordinary objects and falls back to the existing rooted allocator on any miss.
This preserves every collector/rooting invariant while carving out the exact
fast/slow split the machine-code bump path needs.

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

### Current status
First string-storage slice landed without waiting for full trailing-object GC:
`JsStringBody` now stores short flat UTF-16 and Latin-1 payloads directly inside
the existing fixed-size body (`InlineFlat` / `InlineLatin1`) and only reserves
off-slot heap budget / allocates a `Vec` for longer strings. The body and repr
sizes are guarded so the common fixed cell does not grow. This removes the side
malloc for short property names, atoms, literal strings, and substring results
that fit the inline caps while leaving cons/sliced tracing and long-string
storage unchanged.

The Latin-1 policy now applies to every constructor that can prove the code
units fit in one byte: `from_str`, rooted `from_str_with_roots`, and
`from_utf16_units[_with_roots]`. ASCII still uses the borrowed byte slice
directly; non-ASCII Latin-1 and UTF-16-unit callers compact through a temporary
byte vector so the persistent GC payload stays Latin-1.

Dense arrays still use a `Vec<Value>` because the current baseline JIT reads
that vector's probed pointer/length layout for inline dense element access, but
the array shell no longer pays for sparse/named/accessor/symbol/source/prototype
metadata in every dense array. Full variable-size GC trailing storage remains
the deeper D3 target.

## D4 — `Math.sqrt`/`sin`/`cos` bridge to libm instead of a native instruction

### Evidence
```
benchmarks/micro/sqrt.js   10M Math.sqrt   →  otter 1748 ms   node ~5 ms compute  (~100×+)
```
~175 ns per `Math.sqrt` call. It is dispatched as a normal method/global call →
native function → `libsystem_m` (libm). nbody and mandelbrot lean on `sqrt`.

### Landed redesign
**Guarded `Math.<method>(...)` intrinsic opcode.** Direct calls to known
`Math` methods lower to `Op::MathCall` when `Math` is not lexically shadowed.
At runtime the opcode checks that the realm global still points at the
bootstrap `Math` object and that the selected method still points at the
original native function. Primitive arguments run through the numeric dispatch
table directly; object-like arguments fall back to the ordinary method-call path
so user `@@toPrimitive` / `valueOf` / `toString` hooks remain observable.

The baseline JIT delegates `Op::MathCall` through the runtime opcode bridge,
which is deliberately conservative: it removes the hot property/method/native
call bridge while preserving global replacement, method overwrite, lexical
shadowing, and object coercion semantics. A future machine-code `fsqrt`/`fabs`
fast path can build on the same guard contract rather than speculating
unconditionally.

Measured on `benchmarks/micro/sqrt.js` with `OTTER_STATS=1` against clean
pre-change HEAD `8e4e02f96c9ebbc710b4d67df90205de68daaaf0`:
- JIT on: **1713 ms → 698 ms**, `nativeCalls` **10,000,182 → 184**,
  `jitRuntimePropertyStubs` **19,995,996 → 0**.
- `OTTER_JIT=0`: **3915 ms → 2987 ms**, `nativeCalls`
  **10,000,182 → 184**.

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
| 1 | **R1** self-priming property IC — LANDED | JIT stub + IC fill | 5–9× on top-level code | de-risks all IC work |
| 2 | **VmError shrink** — LANDED | boxed string payloads + 24B guard | 5–15% broad | cleaner profiles |
| 3 | **D4** Math.* intrinsics — LANDED guarded opcode | compiler + VM/JIT delegate | float benches | safe guard contract for native emit |
| 4 | **R3** inline array `.length` | emit + array header | kills per-iter length stub | every array loop |
| 5 | **D1+R2** object-model rewrite: split god-struct + flat slab | ObjectBody, GC tracing, body offsets | ~13× alloc, ≈7–10× smaller objects, fewer GC cycles | json/prop/array/OO |
| 6 | **D2** inline bump-alloc in JIT — in progress | emit + nursery cursor | per-`new` floor gone | direct stub landed; raw bump remains |
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
