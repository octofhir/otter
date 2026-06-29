# VM Refactor Plan — Aggressive (JIT untouched, off by default)

**Scope.** Improve the **VM itself**. JIT **off by default, NOT touched** this
phase. **No back-compat constraint** — bytecode, object layout, frame model,
value-side ABI, macros all break freely. Whole-subsystem rip-and-replace, not safe
incremental slices. The only invariants kept are *behavioral* (diff + test262) and
*GC soundness* (no use-after-move). Everything structural is on the table.

Companion: `VM_ABI_AUDIT.md`.

## Result targets (the point of the exercise)

Reproduced baseline (JIT off, Richards 80 runs, 2026-06-29): `durationMs=1818.8`,
`reductionsExecuted=164.1M`, `bytecodeCalls=3.24M` → **50.7 reductions/call**,
`propertyLoad+StoreHits=20.2M` (each a shape-id + atom-id + `eq_str` name check).
Node 24 on the same loop ≈ 160 ms.

Targets for this phase (interpreter only, JIT still off):

| Metric | Now | Target | Mechanism |
|---|---|---|---|
| Richards `durationMs` | 1818 | **≤ 600** (≤3.7× node) | all four phases |
| `reductionsExecuted` | 164.1M | unchanged* | (per-op cost is what drops, not op count) |
| ns / reduction | ~11.1 | **≤ 3.5** | flat regs + shape→slot + thin Result |
| property name (`eq_str`) checks / run | 20.2M | **0 on hits** | shape→slot |
| `trace_roots` share | 10–13% | **≤ 3%** | card-authoritative minor GC |

*Reduction *count* is fixed by the bytecode; the win is ns/reduction. If a phase
also cuts op count (e.g. fused property ops), even better.

## Ground rules

- **JIT off by default, untouched.** No `otter-jit` edits. Default path =
  interpreter; `OTTER_JIT=1` kept only to prove JIT conformance unchanged. Flipping
  the default is a one-line default change, not a new flag.
- No new flags / `OTTER_*` toggles / `thread_local` / process-global caches. One
  default path; revert via git.
- **No fallback paths left behind.** When a representation is replaced, the old one
  is deleted, not gated. Dictionary mode is the *only* sanctioned slow path and it
  stays explicitly isolated.
- Preserve moving-GC + manual rooting + exact-PC deopt (so a later JIT effort needs
  no VM change — but that effort is out of scope now).
- oxc ASTs for JS/TS parsing; never regex-parse.

## Verification (every landed slice — non-negotiable, the aggression is in the code not the testing)

```bash
cargo build --release -p otter-cli
node benchmarks/diff.mjs                                   # 24/24 identical, all tiers
just test262 2>&1 | tee /tmp/after                         # FAILING-SET identical vs baseline
OTTER_GC_STRESS=128 ./target/release/otter run benchmarks/scripts/richards.js
OTTER_JIT=0 OTTER_STATS=1 ./target/release/otter run benchmarks/scripts/richards.js   # thermometer
```

A bottleneck/win claim needs a counter + profile pair (`OTTER_STATS=1` +
`samply`/`atos`). The interpreter is the thermometer.

---

## Phase 0 — Value reencoding to JSC pointer-cheap layout (DECIDED)

**Aggression / why.** Our hot gaps are object/property benches (tree 41×, prop-access
8×), and the current `0x7FFC` NaN-box makes doubles free but **every pointer deref
pay an unmask**. Flip to the **JSC encoding: pointers stored verbatim (top16 = 0),
deref needs NO unmask; doubles offset by `+2^49`; int32 tagged**. This makes every
`LoadProperty` / method-IC / butterfly deref a raw load — compounds across every IC
hit. Doubles pay a `±offset` on box/unbox (we are already competitive on float
benches, so this is the right trade). Foundational: it is the bedrock every later
phase (slabs, ICs, guards) builds on, so it lands FIRST.

**Encoding (mirror JSC `JSCJSValue.h`).**
- `NumberTag = 0xfffe_0000_0000_0000`, `OtherTag = 0x2`, `BoolTag = 0x4`,
  `DoubleEncodeOffset = 0x0002_0000_0000_0000` (= 2^49).
- **Cell pointer**: full 48-bit address verbatim, top16 = 0. `is_cell = (v &
  (NumberTag|OtherTag)) == 0` (and non-empty) — deref is `*(v as *T)`, no mask.
- **Int32**: `v = NumberTag | u32(i)`; `is_int32 = (v & NumberTag) == NumberTag`
  (1 AND + 1 CMP).
- **Double**: `v = f64.bits + DoubleEncodeOffset`; decode `bits(v - offset)`;
  `is_number = v & NumberTag != 0`.
- **Immediates**: null/undefined/false/true/hole via `OtherTag` low bits.

**Registers hold FULL decompressed pointers; heap stays compressed (P1).** A pointer
`Value` in a register/stack slot is the real `cage_base + offset` address (top16 =
0, free deref). The 32-bit compression lives only in heap slabs (P1), decompressed
to a full-pointer `Value` on load. This is the V8/Hermes split + JSC encoding,
coherent.

**What ships / files.** Rewrite `value/tag.rs` + `value/mod.rs` (every `is_*`/`as_*`
accessor, `number_i32`/`number_f64`/`from_object_gc`/box-unbox helpers). **Key
ripple:** constructing a pointer `Value` now needs `cage_base` (full address), so
`Value::from_object(gc)` reads `gc.as_ptr()` — thread the heap/cage where pointer
Values are built (the heap is already threaded via `read_payload`/`with_payload`).
otter-jit consumes the tag constants from otter-vm (JIT stays OFF and untouched, but
must keep compiling — update its baked guard immediates / `debug_assert`s; do NOT
improve JIT codegen).

**Breaking.** The entire `Value` bit layout. Everything that touches bits.

**Invariant.** Pointer deref needs no unmask (top16 = 0, verbatim); every guard ≤2
instructions; doubles pay `±offset` only at box/unbox. Cage MUST sit in low-48-bit
VA so pointers have top16 = 0 — assert at cage init (true on arm64/x64, ≤48 VA bits).

**Verify.** diff 24/24 (Value is internal — zero observable change); test262
failing-set identical; Richards/tree JIT-off `samply` shows the pointer-unmask gone
from `LoadProperty`/method paths; **measure the float micro (nbody/mandelbrot)
regression and confirm it is bounded/acceptable** (the conscious trade). Static
`size_of==8`/`align==8` asserts + full test262 catch a missed bit-site.

**Risk.** High churn but mechanical. Hazard = a missed bit-manipulation site →
caught by test262 failing-set diff. The cage-low-VA assumption is load-bearing —
assert it.

---

## Phase 1 — Object model rewrite: flat shape→slot, split the god-struct

**Aggression.** Stop patching the IC path. Rewrite `ObjectBody` storage so a
property access is `shape-id guard ⇒ decompress(slab[slot])` (32-bit slots, P0/§1
encoding) with **no name string and no atom-id ever touched on a hit**, and split
the ~519-byte god-struct so an `{a,b}` literal is small.

**What ships.**
1. **Slab keyed purely by shape.** Slot offset comes from the shape (`values_ptr`
   base already exists, `object.rs:493`). Fix the R2 cliff (`memory:
   engine_perf_audit`) so **overflow slots are equally IC-cacheable** — not by one
   uniform realloc-on-add slab, but by the principled fixed-inline + out-of-line
   overflow split in item 4 where the IC handles "inline → 1 load, out-of-line → 2
   loads." Delete `body_key_matches`/`eq_str` (`object.rs:~1660`,
   `string/gc_body.rs:612`) from the hit path entirely.
2. **God-struct split** (`memory: engine_perf_audit` D1). `ObjectBody` hot core =
   `{ shape, proto, flags, values_ptr }`; everything else (dictionary_keys,
   dictionary_index, symbol_props, host_data, boolean/number/string/date/error/
   raw_json/arguments wrappers — `object.rs` ~25 fields) moves to a lazily-allocated
   `Box<ExoticData>` reached only by non-ordinary objects. Ordinary literal alloc
   drops from ~578 ns to bump+slot-init.
3. **One shape contract.** `AtomOwnPropertyHit` (`object.rs:425`) collapses to
   `{ shape_id, slot }` — no `atom_id`. Shape-id match is *sufficient*. Compare the
   **32-bit ShapeID as an immediate** (JSC StructureID), not a 64-bit pointer.
4. **Fixed inline slots + out-of-line overflow — do NOT realloc the body**
   (research: V8 `inobject_properties`, JSC butterfly, SM `numFixedSlots`). Cap
   inline slots at construction; spill the rest to a separate growable array. A slab
   we `realloc` on every add relocates the object and breaks IC offset stability —
   replaces the deleted `INLINE_VALUE_CAP` cliff with a *principled* boundary the IC
   handles as "inline → 1 load, out-of-line → 2 loads."
5. **Separate elements (integer-indexed) from named properties** into distinct
   backing stores (V8/JSC). Indexed writes must not thrash named-property shapes.
6. **Shared BaseShape** = interned `{class, realm, proto}` (SM/JSC) so proto/class
   stay out of the transition tree. Always advance the shape via define, **never
   dict-flip the prototype** (our known fast-shape-killing bug). Hot/cold split:
   exotic/optional fields → keyed side-table (Nova), not inline.

**DECIDED — 32-bit compressed slabs + boxed heap doubles** (V8/Hermes). Object slots
are **32-bit cage-relative compressed** values, not 8-byte `Value`s: slab density
×2, GC scan bytes ÷2. The slab edge gets a `compress(Value)->u32` /
`decompress(u32)->Value` pair (decompress = `cage_base + offset` → full-pointer
`Value` per P0; small-int inline via low-bit tag in the 32-bit slot; **f64 doubles
BOXED on the heap** as a `HeapNumber`-equivalent and referenced by compressed
pointer). Property read = `shape-id guard ⇒ decompress(slab[slot])` (`slot*4`).
`cage_base` pinned in a register across the interpreter loop. The IC caches the
32-bit compressed shape offset as its key.

**Breaking.** `ObjectBody` layout, IC hit structs, dictionary transition path,
every `read_payload`/`with_payload` site that touched the moved exotic fields. GC
trace must walk the slab + lazy `ExoticData`.

**Invariant.** `shape_id(recv) == baked ⇒ slot valid`. Name strings only on
miss/install. Dictionary mode (null shape) is the lone isolated slow path.

**Verify.** diff 24/24; full test262 `built-ins/Object`, `language/.../property-*`,
proxy, accessors failing-set identical; `OTTER_GC_STRESS=128`; Richards JIT-off
`durationMs` + `propertyLoadHits` profile shows `eq_str` gone.
**Result gate:** Richards `eq_str`/name-check share → 0; `samply` no
`load_own_data_slot_atom` string work on hits.

**Risk.** High (object model is everything). Hazard = shape-id reused with a
different slot map → silent wrong-slot read. Debug-only assert that resolved
slot's key equals requested key on every hit (compiled out of release) + the
attribute/accessor test262 suites are the net.

---

## Phase 2 — One flat value stack + zero-copy frames + interpreter hot path

**Aggression.** Delete `Frame.registers: Vec<Value>`. ONE flat per-isolate `Value`
stack; frames are windows into it; interpreter and (off, future) JIT share that
single stack — no parallel `reg_stack` (the `JitCtx.reg_stack_base`/`reg_top_ptr`
duplication goes away, only one stack). Register read = one indirection `base+j*8`.
Layout copied from the shipped consensus (Hermes/JSC/Boa/BEAM, `VM_ENGINE_RESEARCH.md`
§4). **NOTE: dispatch is NOT rewritten — research retired that idea** (tail-call
threading needs nightly `become`/`preserve_none`, ~1–5% payoff; register-VM under a
`match` already banks Ertl's 1.48×). The win is the flat stack + zero-copy calls +
stable-Rust hot-path levers, not a dispatch-loop rewrite.

**What ships.**
1. **Flat stack.** `HoltStack` (`holt_stack.rs:179`) backing → single `Vec<Value>`
   (pre-reserved `DEFAULT_MAX_STACK_DEPTH * max_regs`, reservation-stable — throw
   stack-overflow before realloc). `Frame` (`frame_state.rs`) drops its `registers`
   Vec; holds `base_off`, `reg_count`, cold metadata.
2. **Zero-copy calls (Hermes/JSC straddling frame).** Frame metadata + incoming
   args laid so the callee's arg registers ARE the caller's top registers — no
   argument copy per call. Kills the "per-call frame build = fib 56%" tax. Header
   (caller frame, return PC, callee, argc, this) at fixed offsets; locals adjacent.
3. **BEAM X/Y register split.** A small global **temp bank** (caller-saved, reused
   every call, never spilled into a frame) for transient operands + arg staging;
   **frame slots** only for values live across a call/yield/GC point. The compiler
   already has the liveness (reg-alloc) to route temps to the bank — shrinks frames,
   cuts push/pop, and shrinks the safepoint root set (feeds P4 stack-maps).
4. **Register access via cached base pointer** (`lib.rs:7012 dispatch_loop_inner`):
   `base + operand*8`, no `stack[i].registers[j]` double deref. Keep the existing
   per-frame cache (`memory: dispatch_loop_perf`), feed it the flat base.
5. **Stable-Rust interpreter levers** (`VM_ENGINE_RESEARCH.md` §7), pure-stable, no
   dispatch rewrite: **load-time pre-decode** to a typed `Vec<DecodedInsn>` (operands
   already register indices/immediates, dense opcode enum); **verify→`get_unchecked`**
   register access (validate operand `< nregs` at load, document the unsafe
   invariant); **superinstructions** for hot opcode pairs from profile; optional
   **accumulator** register; `#[cold] #[inline(never)]` for slow arms.
6. **Single GC root scan.** `trace_active_frame_roots` (`frame_roots.rs`) walks the
   flat stack once `[0, top)`; generators/async that suspend off-stack copy their
   window out (cold path). Sets up P4's stack-map rooting.

**Breaking.** Every call/return (`call_ops.rs`, `frame_ops.rs`), generator/async
suspend-resume (frame survives off-stack), arguments object, `trace_frame_slots`,
the GC frame-roots provider.

**Invariant.** One value stack per isolate; register read = one indirection; frame
offsets stable for frame lifetime; GC scans the flat stack exactly once.

**Verify.** diff 24/24; **full** test262 failing-set identical (generators/async/
arguments are load-bearing — run them explicitly as a sub-gate); `OTTER_GC_STRESS=128`;
Richards JIT-off `durationMs` + `samply` shows `HoltStack::index`/`index_mut` gone.
**Result gate:** ns/reduction drops materially (the register-deref tax).

**Risk.** High — touches every call path + generators + GC roots. Reservation-stable
must hold exactly (a realloc dangles live frame pointers). Sub-gate generators/
async on their own conformance run before merge.

---

## Phase 3 — Thin `Result`, cold-error, hot-coercion sweep

**Aggression.** `Result<Value, VmError>` on every op must be pointer-sized. Box the
fat error; the success path never builds or drops error data.

**What ships.**
- `VmError` (`run_control.rs:97` + enum def) → thin: `Box` the cold payload so
  `Result<Value, VmError>` is 8–16 B. Keep `NativeError::Coded{code:&'static str}`
  for Node `ERR_*`. Eager `ok_or` → `ok_or_else` on hot constructors
  (`memory: jit_design_plan`, 095d6b41: the success-path error drop was ~25% interp
  self-time).
- Confirm ToNumeric/ToPrimitive elision already mined (`memory: jit_design_plan` —
  done); no new work unless profile says otherwise.

**Breaking.** `VmError` shape — but error *messages* must stay byte-identical
(test262 asserts them).

**Invariant.** Interpreter success path never materializes/drops a fat error.

**Verify.** diff 24/24; test262 failing-set identical (message bytes are the risk);
`samply` shows `drop_in_place<VmError>` gone from interp self-time on string-ops +
Richards. **Result gate:** 5–15% interp self-time recovered, broad.

**Risk.** Low-medium; the only hazard is an altered error message — full test262
failing-set diff catches it.

---

## Phase 4 — Generational remembered-set GC (ship the algorithm, not just telemetry)

**Aggression.** Make the remembered set authoritative for old→young and stop the
broad per-minor root re-scan now. Telemetry lands in the same phase as the proof,
not as a separate timid step.

**What ships.** (research sharpens the original card-table plan — `VM_ENGINE_RESEARCH.md` §5)
1. GC telemetry counters (`heap.rs collect_minor_internal`, `scavenger.rs`): pause
   totals/max, root slots per category, dirty slots scanned, objects traced, young
   copied/promoted. Land first within the phase to prove the split.
2. **Precise SlotSet remembered set, NOT a card table** (V8 field-logging). 1 bit
   per pointer-slot, 1024-bit buckets, lazily allocated, ~3% overhead; minor GC
   iterates set bits → *exact* old→young slots. This **avoids the swept-corpse
   use-after-free bug class** our card-table dirty-walk produced
   (`scan_old_dirty_cards`) — no walking arbitrary headers on a dirty card. With
   32-bit compressed slots each entry is 4 bytes, bit-density doubles.
3. **Safepoint stack-map rooting** (V8/Hermes/BEAM — the per-call-rooting-tax cure).
   The compiler emits live-register metadata at each GC-safepoint/call/alloc op;
   GC scans only the live window of the flat stack (P2), discovered lazily at GC,
   **zero bookkeeping on the hot call path**. Replaces incremental per-call
   `HandleScope` rooting and kills the use-after-move hazard class — while KEEPING
   moving + compression. (The §6 fork: this is the recommended path vs JSC
   conservative+non-moving.)
4. **Two-level Smi-skip write barrier** (V8): (1) bail if value is small-int
   (NaN-box test, no decompress); (2) test "from-here-interesting" page-header bit;
   (3) only then record the slot into the SlotSet. Inline fast path (no bridge),
   shared slow-path stub. Cheaper than the unconditional young/old card-set.
5. **Off-heap refcounted large blobs** (BEAM/JSC): large strings/ArrayBuffers off
   the moving heap so the copier never relocates multi-MB payloads.
6. Minor collection roots = young space + SlotSet dirty slots + the stack-map live
   window — never the full stable root set every cycle (`memory:
   codegen_quality_gap_map`: `trace_roots_inner` 10–13% on all alloc benches).

**Breaking.** Minor-GC root algorithm; root-category bookkeeping in
`runtime_state.rs`. Moving-GC safety preserved.

**Invariant.** A minor GC never re-scans the entire stable root set; old→young
edges come from the SlotSet + the stack-map live window.

**Verify.** GC invariant tests + `OTTER_GC_STRESS=128` (the collector has
use-after-move history — `memory: gc_architecture_decision` — this is the GC-risk
phase); no new use-after-move; diff 24/24; alloc-bench pause totals before/after.
**Result gate:** `trace_roots` share ≤3% on map-set/string/tree.

**Risk.** Highest (moving collector). Telemetry-first *within* the phase, algorithm
second; never ship a regressing `OTTER_GC_STRESS`.

---

## Phase 5 — Cross-cutting: one IC IR (CacheIR) + work-proportional metering

Two research-driven additions that span the other phases (`VM_ENGINE_RESEARCH.md`
§3, §8).

**5a — CacheIR-style single IC IR (build now, JIT off).** A **linear guard IR**
(`GuardShape Op0 Field0; LoadFixedSlotResult Op0 Field1`) with concrete
`Shape*`/slot-offset in per-stub **"stub data," not baked into the IR**. The
interpreter executes it via a tiny IR interpreter; the future optimizing JIT lowers
the *same* IR. SM's biggest lesson: never write interpreter ICs and JIT ICs twice
and keep them in sync — the exact divergence class that produced the JIT crash
(`VM_ABI_AUDIT.md` D), applied to ICs. Moving-GC synergy: constants in stub data
make IC roots enumerable/traceable (one updatable `Shape*` per stub). Replaces the
ad-hoc `property_ic.rs` / `method_ops.rs` IC structs with one representation.
*Verify:* diff 24/24 + test262 invariance; interpreter IC hit-rate unchanged; the
IR is the contract the deferred JIT will consume.

**5b — Work-proportional reduction metering (BEAM).** Charge reductions
proportional to work for O(n) native/builtin calls (string ops, JSON, regex, big
array copies) instead of a flat 1; make long native loops trap/resume. Keeps one
fat `JSON.parse` from blowing a preemption window. Check the budget at call/back-edge
opcodes. *Verify:* diff 24/24; preemption-latency telemetry; no conformance change.

---

## Order & independence

```
P0 (Value reencoding) ── BEDROCK, lands first; P1 slabs + all guards depend on it.
   │
   ├─ P3 (thin VmError) ── cheap/broad, independent
   ├─ P1 (object model + 32-bit slabs) ── lands CacheIR IR (5a) as IC substrate
   ├─ P2 (flat reg stack) ── P4 stack-maps depend on it
   ├─ P4 (GC SlotSet + stack-map rooting)
   └─ P5 (5a IC IR with P1, 5b metering)
```

All VM-internal, JIT-off-measurable, independently shippable. Recommended sequence
by ROI/risk: **P0 → P3 → P1 (+5a) → P2 → P4 → 5b**. P0 is bedrock (the value
encoding every slab/guard/IC builds on); P1's 32-bit slabs assume P0's full-pointer
registers + compress/decompress edge; P4's stack-map rooting needs P2's flat stack.

**Decisions — SETTLED 2026-06-29** (`VM_ENGINE_RESEARCH.md`):
1. **32-bit compressed object slabs + boxed heap doubles** — YES (P1). Slab density
   ×2, GC scan bytes ÷2; doubles boxed on heap.
2. **Flip to JSC pointer-cheap NaN-box** — YES (P0, lands first). Pointers verbatim
   (top16=0, free deref); doubles `±2^49` offset; the conscious trade is float
   box/unbox pays an ALU op (we are already competitive on float benches).
3. **precise + moving + stack-maps** — CONFIRMED (P4). Keep compaction +
   compression; cure the per-call rooting tax via safepoint stack-maps. JSC
   conservative+non-moving is NOT taken (it forfeits compaction + stack-reachable
   compression).

---

# Appendix — Deferred: JIT boundary (do NOT touch this phase)

Audit areas D / E / F (`VM_ABI_AUDIT.md`) — the JIT↔VM ABI. **Out of scope: no
`otter-jit` edits, no opt-tier re-enable.** Recorded so a later, separately-scoped
JIT project starts from the audit. The four VM phases above deliberately preserve
exact-PC deopt + reservation-stable frames + the shape→slot contract, so that
future effort needs no further VM change.

## Deferred D — One compiled-entry contract; delete the `direct_*` shadow set

The crash class. `JitCtx` (`baseline.rs:96`) is 25 fields + an 8-field `direct_*`
shadow set (`baseline.rs:130-147`), hand-copied by two diverged
`emit_direct_call_tail` bodies (`baseline.rs:4702`, `optimizing/emit.rs:1565`). The
optimizing tail omits gc_heap/safepoint/collection_ics/protector that the baseline
copies → garbage callee ctx → SIGSEGV (`memory: richards_fid27_crash_rootcause`).
Target: split into a per-frame `JitEntryFrame` + a per-isolate `JitBoundaryView`
referenced by ONE shared pointer (never copied per call), built by ONE
`emit_enter_callee` helper consumed by both tiers. Deletes both tails, the 8
`direct_*` fields + offsets, `reject_call_object_mix`'s CallMethod clause.

## Deferred E — Register-map safepoints + single frame-state descriptor

`TaggedLocation` already supports MachineRegister/SpillSlot (`native_abi.rs:565`)
but only `frame_slot_window` (`native_abi.rs:632`) is ever baked → every live value
materialized to the tagged frame window at allocating ops (the fib boxing tax).
Target: liveness-driven `TaggedLocation` maps over regs+spills+frame slots, one
`FrameStateId` per safepoint/guard for exact-PC deopt; `frame_slot_window` kept
only as conservative fallback.

## Deferred F — Single `JitGcView` boundary surface

`JitCtx.gc_heap` is opaque `*const c_void` (`baseline.rs:165`) and the card/young/
page layout is re-derived inside the JIT view and `debug_assert`'d against otter-gc
(`ENGINE_REWORK_TRACKING_PLAN.md:210-226`). Target: one published `JitGcView`
(`heap_ptr`, `cage_base`, card-bitmap offset, page mask, card shift, `FLAG_YOUNG`,
GcHeader offsets) owned by otter-vm; pointer-reconstruct + card-mark + nursery-bump
emit inline from it. `cage_base` also serves the value-side pointer reconstruction
(audit area A).

## Deferred 6 — Re-enable the optimizing tier

After a later D+E+F land, turn the tier on: the divergent hand-built tail no longer
exists (one `emit_enter_callee`), so the fid-27 crash class is structurally gone.
Until that separate project is scoped, JIT stays off by default and untouched.
Positive proof at that time: Richards RC=0 + `richards=260000` with the tier on,
`jitDirectCalls` ≫ 260,060, diff 24/24, test262 `OTTER_JIT=1`==`OTTER_JIT=0`,
`OTTER_GC_STRESS=128 OTTER_JIT=1` + the deopt suite.