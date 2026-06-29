# VM ↔ JIT ABI Audit

Evidence-backed audit of the otter-vm ↔ otter-jit boundary.

**Scope directive.** The current phase improves the **VM itself**; the JIT is
**off by default and NOT touched**. Areas A / B / C are the VM-internal work and
drive `VM_JIT_FRIENDLY_REFACTOR_PLAN.md`. Areas D / E / F audit the JIT boundary
and are documented here for a **later, separately-scoped** effort — they are
**not** worked in this phase. The correctness reference throughout is
`OTTER_JIT=0`.

## Reference measurements (reproduced this session)

Host: macOS arm64, `target/release/otter` (release + debuginfo), 2026-06-29.
Workload: `benchmarks/scripts/richards.js` (80 `runRichards()` calls, `checksum=260000`).

| Metric | `OTTER_JIT=0` (interp) | `OTTER_JIT=1` |
|---|---|---|
| `durationMs` | 1818.8 | 1745.0 |
| `reductionsExecuted` | 164,112,117 | 145,806,978 |
| `bytecodeCalls` | 3,237,311 | 2,977,175 |
| interp reductions / call | 50.7 | 49.0 |
| `propertyLoadHits` | 16,149,557 | 14,939,048 |
| `propertyStoreHits` | 4,047,081 | 3,273,211 |
| `jitDirectCalls` | 0 | 260,060 |
| `jitMethodGenericCalls` | 0 | 76 |

Two headline facts fall straight out of the table:

1. **JIT pays ~4% on real OO code** (1745 vs 1818 ms). Tiering does not earn its
   keep on Richards.
2. **Only ~8.7% of calls enter compiled code** (`jitDirectCalls` 260,060 of
   `bytecodeCalls` 2,977,175). The method *bodies* interpret — ~49 reductions per
   call — behind the polymorphic `this.task.run()` site.

`node benchmarks/diff.mjs`: **24/24 identical across interp / jit / jit-osr**.
This is the behavioral gate every refactor slice must hold.

---

## A. Value representation — already JIT-friendly

**Current design.** `Value` is a `#[repr(transparent)] u64` NaN-box
(`crates/otter-vm/src/value/mod.rs:88-90`), 8 bytes, `Copy`, no discriminant, no
refcount. Tag layout (`crates/otter-vm/src/value/tag.rs:13-70`): the high 16 bits
select the tag; doubles are stored verbatim, and the window `0x7FF9..=0x7FFF`
holds INT32 / SPECIAL / FUNCTION_ID / four pointer families. Pointer payloads are
the **32-bit compressed GC offset** (low 32 bits; bits 32..48 must be zero,
`tag.rs:33-36`).

**JIT-friendliness assessment — GOOD, with two friction points.**

- Immediate guards are already ≤2 instructions: int32 test = compare top16 to
  `0x7FF9`; `is_double` = `tag < 0x7FF9 || tag > 0x7FFF` (`tag.rs:121-125`), two
  compares. This is exactly the target invariant ("every value lives in a
  representation a guard can test in ≤2 instructions"). No NaN-box / pointer-tag
  rework is needed — the model is already there.
- Friction 1 — **pointer reconstruction**: a pointer-tagged `Value` carries only
  the 32-bit offset, so compiled code must reconstruct a real address as
  `cage_base + low32(value)` before any field load. `cage_base` is a GC-owned
  quantity not currently exposed on the compiled-entry boundary as a first-class
  field (see area F).
- Friction 2 — **family disambiguation**: `TAG_PTR_OBJECT` (`0x7FFC`) is shared by
  18 body kinds (`ObjectFamilyKind`, `value/mod.rs:114-157`). The tag only selects
  the *family*; distinguishing array vs map vs ordinary object requires a second
  read of `GcHeader::type_tag` (`crates/otter-gc/src/header.rs:76`). A monomorphic
  object guard is therefore: top16 compare + cage-base add + shape load. That is
  acceptable for a JIT, but the JIT must be handed `cage_base` and the header
  layout to emit it inline rather than via a bridge.

**Blast radius of change.** Minimal. No layout change is wanted. The only ABI work
is exposing `cage_base` + `GcHeader` flag/tag offsets on the boundary view
(folded into area F). Touching the tag scheme itself would be a project-wide churn
with no payoff and is explicitly NOT recommended.

**Target invariant.** Keep NaN-boxing, but **reencode to the JSC pointer-cheap
layout** (DECIDED 2026-06-29): pointers stored verbatim (top16=0, free deref),
doubles offset `+2^49`, int32 tagged — because our hot gaps are object/property
benches where the current `0x7FFC` per-pointer unmask costs on every IC hit.
Registers hold full decompressed pointers; heap slabs hold 32-bit compressed
offsets (decompress on load). Publish `cage_base` + `GcHeader` field offsets
(`type_tag`, `flags`) as compile-time-stable boundary constants. See
`VM_JIT_FRIENDLY_REFACTOR_PLAN.md` Phase 0.

---

## B. Object model & property access — name-keyed on the validated hot path

**Current design.** `ObjectBody` (`crates/otter-vm/src/object.rs:483-572`) is
*already* shape-oriented and JIT-shaped: field 0 is the `ShapeHandle` at a fixed
offset (`OBJECT_BODY_SHAPE_OFFSET`) for monomorphic guards, field 1 is a cached
`values_ptr: *mut Value` base for the contiguous string-keyed slab, indexed by
shape slot offset. Inline-cache metadata exists:
`AtomOwnPropertyHit { shape_id, shape, atom_id, slot }`
(`object.rs:425-439`) carries shape + slot offset, exactly what a JIT bakes.

**Why it is JIT-hostile.** The *validation* on the IC-hit path is still
name-keyed. `load_own_data_slot_atom` (`object.rs:1726-1752`) re-checks
`body_shape_id(...) != hit.shape_id` **and** `key.atom().id() != hit.atom_id`
**and** `body_key_matches(...key.name())`. `body_key_matches`
(`object.rs:~1660`) calls `shape_body::shape_key_matches_str(heap, shape, offset,
key)` — a **string compare** (`crates/otter-vm/src/string/gc_body.rs:612
eq_str`). So even a *hit* does shape-id check + atom-id check + a key string
compare, when a pure `shape_id → fixed slot offset` lookup would be sufficient
(the shape already implies the key is at that slot). `store_own_data_slot_atom`
(`object.rs:1758-1799`) does the same `body_key_matches` name compare before the
store.

This is the residue the engine-perf audit catalogued as cliffs R1/R2
(`memory: engine_perf_audit`): the inline IC is parasitic on interpreter warming
and overflow slots (≥7th own property) fall out of the inline path entirely.

**Evidence (counter).** On the interp Richards run, `propertyLoadHits =
16,149,557` and `propertyStoreHits = 4,047,081` — ~20.2M property accesses, every
one of them walking shape-id + atom-id + a name compare. The deeper-profile note
in `ENGINE_REWORK_TRACKING_PLAN.md:154-158` independently resolves Richards
self-time to `object::load_own_data_slot_atom` + `string::eq_str`.

**Blast radius.** Medium-high but contained. Property load/store/has opcodes in
the interpreter (`property_dispatch.rs`, `property_ic.rs`), the IC hit structs in
`object.rs`, and the shape-key matcher. No object *storage* layout change is
required — the slab + shape already exist. The change is to make the validated
path key on `(shape_id == baked_shape_id) ⇒ slot offset` and demote the name
string only to the IC-miss / first-fill path. This is fully observable with JIT
off (interpreter reductions/wall on Richards).

**Target invariant.** Property access is keyed by `shape-id → slot-offset`. The
name string is touched only on IC miss / install. The same `{shape_id, slot}`
contract is what a JIT bakes into a monomorphic/polymorphic guard, so the
interpreter IC and the compiled IC share one shape contract.

---

## C. Frame / register window — two indirections per register read

**Current design.** Frames live on `HoltStack`
(`crates/otter-vm/src/holt_stack.rs:179-183`), a `#[repr(transparent)]` over
`SmallVec<[Frame; 8]>` pre-reserved to `DEFAULT_MAX_STACK_DEPTH` so the buffer
never reallocates (reservation-stable — a genuine prerequisite for compiled
callees holding raw caller-frame pointers, and correctly designed for that). Each
`Frame` holds its register window as a separate heap allocation
(`frame.registers.as_mut_ptr()`, `holt_stack.rs:142-144,268-270`).

**Why it is JIT-hostile / interp-hostile.** A register read is two indirections:
`stack[i]` (SmallVec deref + bound) then `frame.registers[j]` (Vec deref + bound).
The dispatch loop reads registers on essentially every operand of every op.
`std::ops::Index`/`IndexMut` for `HoltStack` (`holt_stack.rs:358-372`) and the
`Frame.registers` Vec are both on that hot path. The window holds **tagged**
`Value`s because it doubles as the conservatively-scanned GC root set and the
deopt frame-state source, which forces every compiled self-call to box→store→
load→unbox each live value *through this tagged memory window* per call (see area
E and `memory: codegen_quality_gap_map`, the fib 3× ceiling).

**Evidence.** `reductionsExecuted = 164,112,117` over `bytecodeCalls =
3,237,311` (interp) = **50.7 interpreted reductions per call**; the method bodies
run here. `ENGINE_REWORK_TRACKING_PLAN.md:29-31,154-158` resolves Richards
self-time to `dispatch_loop_inner` + `HoltStack::index`/`index_mut` (register
reads) via `samply`/`atos`. `dispatch_loop_inner` is `crates/otter-vm/src/lib.rs:7012`.

**Blast radius.** High — `dispatch_loop_inner` and every call/return path
(`call_ops.rs`, `frame_ops.rs`, `frame_state.rs::Frame`). But it is interpreter
substrate, so the win is JIT-off measurable. The reservation-stable property must
be preserved exactly (compiled callees depend on it).

**Target invariant.** One indirection to a register slot: a single flat
per-isolate value stack with frames as `(base, len)` windows into it, so a
register read is `base + j*8` with no per-frame Vec. (The flat JIT register stack
fields `reg_stack_base` / `reg_top_ptr` in `JitCtx` already gesture at this — the
interpreter frame model should converge onto the same flat stack rather than
maintain a parallel `Frame.registers` Vec.)

---

## D. Call convention & the JIT boundary — a 25-field god-struct with a hand-copied shadow set, already diverged

This is the crash class and the structural reason to refactor before adding
optimizing-tier features.

**Current design.** The entire VM↔JIT contract is the single `JitCtx` struct
(`crates/otter-jit/src/baseline.rs:96-171`) — **25 fields** spanning regs / self /
this / bail_pc / vm / stack / context / frame_index / upvalues / error / gc_heap /
safepoint records+count / collection-method ICs+count / array-index protector /
flat-reg-stack base+top — **plus an 8-field `direct_*` shadow set** baked into the
same struct for the direct compiled→compiled call path: `direct_entry_addr`,
`direct_regs`, `direct_safepoint_records`, `direct_safepoint_count`,
`direct_self_closure`, `direct_this_value`, `direct_frame_index`,
`direct_upvalues_ptr` (`baseline.rs:130-147`). Every field has a hand-maintained
`*_OFFSET` const via `offset_of!` (`baseline.rs:187-273`).

Two emitters must each hand-replicate the contract by lowering a callee `JitCtx`
on the stack field-by-field:

- baseline `emit_direct_call_tail` (`baseline.rs:4702-4801`),
- optimizing `emit_direct_call_tail` (`crates/otter-jit/src/optimizing/emit.rs:1565-1647`).

`jit_prepare_direct_call` / `jit_prepare_direct_method_call`
(`baseline.rs:288+`, `method_ops.rs`) stage the `direct_*` fields; the callee is
entered with `blr x16` to `direct_entry_addr`.

**Why it is JIT-hostile — and the concrete divergence.** The two tails are
copy-pasted and have **already diverged**. The baseline tail copies, into the
fresh callee ctx (`baseline.rs:4741-4752`):

```
ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET
COLLECTION_METHOD_ICS_OFFSET / COLLECTION_METHOD_IC_COUNT_OFFSET
GC_HEAP_OFFSET
DIRECT_SAFEPOINT_RECORDS → SAFEPOINT_RECORDS_OFFSET / SAFEPOINT_COUNT_OFFSET
```

The optimizing tail (`emit.rs:1574-1599`) **omits all five**. It `sub sp` without
zeroing, so in the optimizing callee `gc_heap`, `safepoint_records`,
`safepoint_count`, `collection_method_ics(+count)`, and
`array_index_accessor_protector_ptr` are **uninitialised stack garbage**. The
moment that callee hits an allocating / safepoint / collection-IC op it reads
garbage and `blr`s / dereferences into an unmapped address — the Richards SIGSEGV.

**Evidence.** `memory: richards_fid27_crash_rootcause` — gating
`jit_prepare_direct_method_call` to `Ok(None)` yields RC=0 + correct
`richards=260000`; the crash is specifically the direct compiled→compiled
*method-call* tail entering an optimizing callee by raw `entry_addr`. The
side-by-side read above (`baseline.rs:4741-4752` vs `emit.rs:1574-1599`) is the
mechanical proof that the two emitters do not agree on the callee ctx. The note
that "copying the missing fields did not fully fix it" only strengthens the
thesis: a 25-field contract hand-replicated in assembly in two places cannot be
kept in sync by patching — the entry contract itself must be singular.

The current masking workaround is `reject_call_object_mix` counting `CallMethod`
(`ENGINE_REWORK_TRACKING_PLAN.md:119-130`), which keeps Richards' dispatch methods
in the baseline/interpreter — correct but slow, and the reason `jitDirectCalls` is
only 260,060.

**Blast radius.** The refactor deletes both `emit_direct_call_tail` bodies, the 8
`direct_*` `JitCtx` fields and their offsets, `reject_call_object_mix`'s
CallMethod clause, and the `jit_prepare_direct_*` staging of `direct_*`. Replaced
by one entry-frame descriptor consumed identically by both tiers. High-risk
(deopt/GC/exact-PC), but it is the single change that makes the crash class
*structurally impossible*.

**Target invariant.** ONE compiled-entry contract: a single `JitEntryFrame`
descriptor (callee regs base, this/self, frame index, upvalues, plus a *single*
shared "isolate boundary view" pointer for gc_heap / safepoints / ICs / protector
— never duplicated per call). Both tiers construct the *same* descriptor through
one shared emitter helper; there is no `direct_*` shadow set and no per-emitter
field list to keep in sync. Register args/results where the SSA backend wants
them; the tagged frame is materialised only at a real deopt exit.

---

## E. Safepoints / deopt frame-state — register-map-capable type, but only frame-slot windows are ever baked

**Current design.** `SafepointRecord` (`crates/otter-vm/src/native_abi.rs:615-622`)
= `{ id, frame_state: FrameStateId, tagged_locations: Vec<TaggedLocation> }`. A
`TaggedLocation` (`native_abi.rs:577-582`) can name a `FrameSlot`,
`MachineRegister`, **or** `SpillSlot` (`TaggedLocationKind`, `native_abi.rs:565-572`).
So the *type system already supports a register-map safepoint.*

**Why it is JIT-hostile.** Every safepoint actually constructed uses the
`frame_slot_window` constructor (`native_abi.rs:632-644`), which maps the whole
register window to `FrameSlot` locations — the `MachineRegister`/`SpillSlot`
variants are never produced. The single bake site is `lib.rs:3751`
(`SafepointRecord::frame_slot_window(...)`); the runtime stubs likewise use it
exclusively (`runtime_stubs.rs:1341,1399,1465,1499`). The docstring is explicit:
"Baseline v1 keeps GC-bearing values in frame slots at allocation boundaries"
(`native_abi.rs:626-631`). Consequence: an allocating compiled op must
materialise every live value back into the tagged frame window before the
safepoint, which is the same box→store→load→unbox tax that caps fib at ~3×
(`memory: codegen_quality_gap_map`). `NO_FRAME_STATE` (`native_abi.rs`) marks
records with no deopt state; `frame_slot_window` is also the only deopt frame-state
shape, so exact-PC deopt currently reconstructs the interpreter frame from the
frame-slot window, not from a register/spill map.

**Evidence.** Type supports three location kinds (`native_abi.rs:565-572`); only
`frame_slot_window` is constructed (grep: every `SafepointRecord::` call site is
`frame_slot_window`). fib boxing-through-frame ceiling is documented in
`codegen_quality_gap_map` (the optimizing self-call round-trips every live value
through the tagged window per call).

**Blast radius.** The safepoint *type* needs no change — only the construction
(real liveness → register/spill/frame-slot map) and the deopt reader
(`optimizing/deopt.rs`, `SafepointRecord::frame_slot_window` bake in `lib.rs`).
Miscompile-sensitive (a wrong tagged-location map = GC reads a non-pointer as a
pointer, or misses a live pointer). Verifiable with JIT off only indirectly
(the type/bake changes must keep diff + test262 identical); the win is realised
when the opt tier re-enables.

**Target invariant.** Safepoints carry a real `TaggedLocation` map over machine
registers + spill slots + frame slots, with one `FrameStateId` descriptor per
safepoint/guard for exact-PC deopt. Allocating compiled ops keep live values in
registers and publish them via the map, not by materialising the frame window.

---

## F. GC rooting at the boundary — barrier exists, but the heap surface reaches compiled code as an opaque pointer

**Current design.** The safe write barrier is `GcHeap::record_write(parent,
value)` → `write_barrier_raw` → `barrier::write_barrier(parent_header, child,
&mut marking)` (`crates/otter-gc/src/heap.rs:1796-1818`). Generational state is a
header flag `FLAG_YOUNG = 0b0000_0100` (`crates/otter-gc/src/header.rs:38,48`) plus
a per-page card bitmap (`CARD_SIZE`/`PAGE_SIZE`, `crates/otter-gc/src/page.rs`).
Compiled code reaches the heap through `JitCtx.gc_heap: *const c_void`
(`baseline.rs:166`, `GC_HEAP_OFFSET`) — an **opaque** pointer handed to native
leaf stubs. Rooting at the boundary is the reservation-stable frame window (area C)
scanned as the root set, plus the module-root / handle stacks on the VM side.

**Why it is JIT-hostile.** Two problems. (1) The heap is opaque on the boundary,
so a barrier or nursery-bump must go through a stub unless the JIT hard-codes the
GC's private layout. The pointer-StoreProperty work already had to bake the card
layout (`offset_of!(PageHeader, card_bitmap)`, `!(PAGE_SIZE-1)`, card shift,
`FLAG_YOUNG`) into `JitFunctionView` by hand and `debug_assert` the bits against
otter-jit consts, because otter-jit has no otter-gc dependency
(`memory: jit_opttier_pointer_storeproperty`, recipe in
`ENGINE_REWORK_TRACKING_PLAN.md:210-226`). That hand-baking is the area-D problem
in miniature: GC ABI bits replicated across the crate boundary with no single
owner. (2) `cage_base` (needed to turn a 32-bit `Value` offset into an address,
area A/F) is not a first-class boundary field either.

**Evidence.** `JitCtx.gc_heap` is `*const c_void` (`baseline.rs:165-166`); the
card/young/page constants are re-derived inside the JIT view and `debug_assert`'d
against otter-gc, per the committed pointer-store recipe
(`ENGINE_REWORK_TRACKING_PLAN.md:210-226`, commit `3cf18d0f`). The barrier itself
is allocation-free and moves no GC (insertion barrier dormant under STW marking),
so it is correct to emit inline — the gap is purely that the *layout* has no
single published surface.

**Blast radius.** Define one `JitGcView` (cage_base + heap ptr + card/page/young
constants + GcHeader offsets) owned by otter-vm and consumed by both tiers; fold
`gc_heap` and the hand-baked `JitFunctionView` GC bits into it. Low behavioral
risk (it is a packaging change of facts already used), but it is the prerequisite
for emitting barriers/nursery-bump/pointer-reconstruct inline without per-emitter
drift.

**Target invariant.** Compiled code receives a single `JitGcView` exposing
`cage_base`, the heap pointer, and compile-time-stable card/page/young/header
constants, so pointer reconstruction, the generational card-mark, and nursery
allocation all emit inline from one published surface — no opaque `c_void`, no
per-emitter re-derivation of GC layout.

---

## Cross-cutting summary

| Area | State | Leverage | JIT-off measurable? |
|---|---|---|---|
| A Value | Already good (NaN-box, ≤2-inst guards) | low | n/a |
| B Property access | Shape+slab exists; **validated path still name-keyed** | **high** (20.2M name-checked accesses/Richards) | **yes** (interp reductions/wall) |
| C Frame/register window | Reservation-stable (good) but **2 indirections + tagged window** | high (50.7 reductions/call) | **yes** (dispatch self-time) |
| D Call ABI | **25-field god-struct + 8 `direct_*` shadows, two diverged emitters** | crash-class blocker | only via diff/test262 invariance |
| E Safepoints | Type is register-map-capable; **only frame-slot windows baked** | high (frame-materialisation tax) | indirectly |
| F GC boundary | Barrier correct; **heap opaque, GC layout re-derived in JIT** | medium (enables inline barrier/bump) | indirectly |

The throughline (areas D + E + F): the boundary publishes the same facts in
multiple hand-maintained places (the `direct_*` shadow set, the per-emitter ctx
field lists, the JIT-side re-derived GC constants), and the optimizing emitter has
already drifted from the baseline. The refactor collapses each to a single
published contract, after which the divergent-hand-built-ABI crash class cannot
recur.
