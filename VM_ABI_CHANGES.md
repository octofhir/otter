# VM_ABI_CHANGES.md — Non-JIT Core ABI Changes for JIT Re-Enable

**Status:** planning. **Scope:** the *non-JIT* VM ABI only. The JIT is frozen and
OFF (it bakes the pre-rewrite `Value`/slab ABI and would emit NaN/garbage if
enabled); re-enable is the *next* project. This document does **not** propose JIT
edits. It specifies the stable contract — byte offsets, encoding constants, op
streams, frame/deopt metadata — that the re-enabled JIT will later *lower* and
*bake*, plus the order in which to land them.

**The bar (non-negotiable, applies to every item):** moving-GC invariants
(Cheney young-gen, in-place 4-byte slot rewrite, `RawGc` cage compression),
manual rooting, and exact-PC deopt frame-state metadata must survive every
change. Breaking changes are encouraged — single binary, no back-compat;
bytecode/layout/`Value`-ABI/macros may break.

**GC mechanics live elsewhere.** The remembered-set redesign (card table →
precise set), the on-heap overflow-slab migration, and the minor-GC scan
rewrite are specified in **`VM_GC_REDESIGN.md`** and are *not* duplicated here.
This document only freezes the **ABI surface** those GC changes expose to the
future JIT (flag bits, slab base semantics, barrier insert sequence), and
sequences the GC work relative to the ABI freeze.

**Gate for every item:** `cargo test -p otter-vm` (644) + `OTTER_GC_STRESS=32/64/128`
+ per-bench interpreter value parity. The `diff.mjs` jit-vs-interp suite is STALE
and is *not* a gate while the JIT is off.

---

## Why "freeze before re-enable" is the whole game

Every production engine the research surveyed bakes a small set of VM facts as
compile-time constants and lowers a small set of VM data structures as a stable
IR. The JIT is only correct if those facts are *final* when it compiles:

| Engine | Object guard word | Slot base it bakes | IC/feedback it lowers | Deopt metadata |
|---|---|---|---|---|
| V8 | `Map` @ offset 0 | in-object `[obj+k]`, `PropertyArray` (on-heap) | `FeedbackVector` → `ProcessedFeedback` snapshot (JSHeapBroker) | `TranslationArray` + `SafepointTable` |
| JSC | `structureID` @ 0 | inline slots / `Butterfly` | `StructureStubInfo`/`AccessCase` | `ValueRecovery` map + bytecode index |
| SpiderMonkey | `Shape*` @ 0 | fixed slots / `slots_`/`elements_` | CacheIR op stream → Warp MIR transpile | `Snapshots` / `RResumePoint` |
| Hermes | `clazz_` @ 0 | `directProps_` / `propStorage_` | `(clazz,slot)` cache entry | (no speculative tier) |
| Otter (today) | `ShapeHandle` @ `OBJECT_BODY_SHAPE_OFFSET==0` | `values_ptr` (`OBJECT_BODY_VALUES_PTR_OFFSET`) | `cache_ir::CacheStub` op stream | **does not exist yet** |

Otter already has the *shapes* of the first three columns. The fourth — exact-PC
deopt + safepoint stack maps over the flat register stack — does **not exist**
and is the single largest gap; it is item **B7** below and must be designed into
the VM *before* the JIT lowers against it, because a moving collector cannot
coexist with an optimizing tier without it.

---

# Section A — Do BEFORE JIT re-enable

These bring the VM ABI to final, JIT-bakeable shape. Recommended landing order is
given in the dependency note after the table; rationale is per-item.

| # | Change | Effort | Risk |
|---|---|---|---|
| A1 | Pin & statically assert the full object-body offset contract | S | low |
| A2 | Freeze in-object property *direction* + the `values_ptr` "always-current base" invariant | S | GC |
| A3 | Sequence the on-heap overflow-slab migration (GC doc) ahead of the `values_ptr` freeze | L | GC/rooting (cross-link) |
| A4 | Promote `CacheStub`/`CacheOp` to a versioned, copy-on-compile feedback ABI | M | deopt/correctness |
| A5 | Add monotone per-slot *representation* to the shape contract | M–L | deopt/correctness |
| A6 | Freeze `Value`/`CompressedValue` encoding constants as baked-and-asserted layout | S | deopt |
| A7 | Define the exact-PC deopt frame-state record + safepoint stack-map ABI | L | **highest: moving-GC + deopt** |
| A8 | Freeze the windowed frame/stack header layout | M | deopt |
| A9 | Freeze `GcHeader` flag-bit assignment + the inline write-barrier ABI face | S–M | GC |

**Recommended order:** A3 (GC prereq, via `VM_GC_REDESIGN.md`) → A1 → A2 → A6 →
A5 → A4 → A8 → A7 → A9. A3 must precede A2 because it changes what `values_ptr`
points at. A8 must precede A7 because the deopt record references frame slots. A9
(barrier ABI face) is last because its mechanism is owned by the GC doc and only
its *frozen bit layout* is needed here.

---

### A1 — Pin & statically assert the full object-body offset contract

**(a) WHAT.** `crates/otter-vm/src/object.rs`. The struct `ObjectBody` (`object.rs:493`,
`#[repr(C)]`) already exposes three named offsets:
`OBJECT_BODY_SHAPE_OFFSET` (`object.rs:637`, asserted `==0` at `object.rs:651`),
`OBJECT_BODY_VALUES_PTR_OFFSET` (`object.rs:642`, asserted `>=8` and `%8` at
`object.rs:652–653`), and `OBJECT_BODY_JIT_PROTO_OFFSET` (`object.rs:648`,
asserted `>=8` and `%4` at `object.rs:654–655`). Add and `const`-assert the two
remaining fields the JIT must address directly: an `OBJECT_BODY_SLAB_LEN_OFFSET`
for `slab_len: u16` (`object.rs:568`) and an `OBJECT_BODY_INLINE_VALUES_OFFSET`
for `inline_values: [CompressedValue; INLINE_SLOT_CAP]` (`object.rs:566`,
`INLINE_SLOT_CAP=4` at `object.rs:574`). Tighten the assertions to *exact* values
(not `>=`/`%`) so an accidental field reorder is a compile error, and add a
`const _: () = assert!(size_of::<ObjectBody>() == 96)` size lock.

**(b) WHY.** V8 (`Map`@0, `Map::GetInObjectPropertyOffset`), JSC
(`structureID`@0, `butterflyOffset()`), Hermes (`clazz_`@0,
`DIRECT_PROPERTY_SLOTS`), SpiderMonkey (`shape_`@0), and QuickJS (`shape`,
re-reads base) all bake object field offsets as immediates after a single guard.
QuickJS's negative example is load-bearing: it caches the slot *offset* but
re-reads the prop *base* each access; Otter chooses the faster path (bake the
base) and therefore must guarantee the base offset never silently moves.

**(c) EFFORT.** S — compile-time asserts and two new `offset_of!` constants.

**(d) RISK.** Low. No runtime behavior changes. The only hazard is *omission*: if
a future body reshuffle reorders fields and an assert is missing, a frozen JIT
bakes garbage — exactly why the assertions must be exact and exhaustive.

**(e) HOW IT SETS UP THE JIT.** `GuardShapeId` lowers to `load [obj+0]; cmp` and
a data-slot load lowers to `load base = [obj+OBJECT_BODY_VALUES_PTR_OFFSET];
load word = [base + slot*4]; decompress`. The size lock lets the JIT bake the
allocation size for inline `New`.

---

### A2 — Freeze the in-object property *direction* and the `values_ptr` base invariant

**(a) WHAT.** `object.rs`. Two decisions, frozen:
1. **Direction.** String-keyed slot `i` lives at forward index `i` in the active
   buffer (`inline_values[i]` while `slab_len <= INLINE_SLOT_CAP`, else
   `values[i]`), reached uniformly through `values_ptr`
   (`push_slot` `object.rs:706–718`; migration-at-cap copies inline → `values` at
   `object.rs:712`). Keep forward, header-relative, slot-index addressing — no
   JSC-style bidirectional butterfly.
2. **The base invariant.** `values_ptr` (`object.rs:503`) is **always current**
   after any move, grow, or shrink. It is refreshed in `refresh_values_ptr`
   (`object.rs:886`) and that refresh is already called on every grow/shrink
   (`object.rs:718`, `object.rs:733`) and on every relocation inside
   `trace_slots_safe` (`object.rs:1113`). Promote "no code path may publish a
   stale `values_ptr`" from an implementation detail to a documented, gated
   **correctness invariant**, and add a debug-time `verify_values_ptr()` checked
   at GC safepoints.

**(b) WHY.** All five engines inline forward at `header + index*slotsize`. Otter's
`values_ptr` indirection has no V8/SM analog (they re-derive the store address
from a tagged field); it is an Otter *advantage* — one uniform load path for
inline and overflow — but QuickJS warns that baking a *base* (not just an offset)
imposes the freshness invariant as a hard contract, not an optimization.

**(c) EFFORT.** S — mostly documentation, a debug verifier, and a gate.

**(d) RISK.** GC. The invariant is a moving-GC invariant: the relocating
scavenger `memcpy`s the body and leaves `values_ptr` aimed at the pre-move inline
array; `trace_slots_safe` must recompute it (it does, `object.rs:1111–1114`).
Any new relocation path that forgets the refresh corrupts a baked JIT load. No
deopt impact.

**(e) HOW IT SETS UP THE JIT.** The JIT bakes a single `values_ptr` load as the
slab base for *every* own-data slot regardless of slot number, with no
inline-vs-overflow branch — the property-access fast path the optimizing tier
lowers. The freshness invariant becomes a JIT-re-enable correctness gate.

---

### A3 — Sequence the on-heap overflow-slab migration ahead of the `values_ptr` freeze

**(a) WHAT.** `ObjectBody.values: Vec<CompressedValue>` (`object.rs:509`) is today
a Rust `malloc` buffer *outside* the 4 GiB cage. The migration to a
cage-allocated, page-resident backing body is **specified in `VM_GC_REDESIGN.md`**
(it requires variable-size GC bodies, which the engine does not support today:
`SafeTraceable` size is `sizeof(ObjectBody)`, `GcHeader.size_bytes` is fixed once
at alloc — `header.rs:79`). This entry exists only to **order** that GC work
*before* A2's `values_ptr` contract is frozen, because it changes the *class* of
memory `values_ptr` points at (off-page `malloc` → in-cage page object).

**(b) WHY.** Every surveyed engine keeps the overflow store on a managed page —
V8 `PropertyArray`, JSC `Butterfly` (in the JSValueGigacage), SpiderMonkey
`slots_`/`elements_` (Nursery/tenured cells), Hermes `propStorage_`
(`VariableSizeRuntimeCell`). It is the precondition that dissolves the off-page
slot wall and lets a write barrier address the *written slot's* page rather than
the parent header (`barrier.rs:80–84` currently derives the card from the parent
header *because* the slot may be off-page; see the module note at
`barrier.rs:72–75`). Full rationale: `VM_GC_REDESIGN.md`.

**(c) EFFORT.** L. **(d) RISK.** GC + rooting + variable-size bodies — the
highest-risk GC change; owned and detailed by `VM_GC_REDESIGN.md`, cross-linked
here only for sequencing.

**(e) HOW IT SETS UP THE JIT.** A page-resident slab base means the JIT's later
*inline* write barrier can compute the written slot's page and mark/insert it
precisely, instead of falling back to a parent-header card — i.e. the barrier the
JIT lowers (A9) becomes slot-precise rather than object-coarse.

---

### A4 — Promote `CacheStub`/`CacheOp` to a versioned, copy-on-compile feedback ABI

**(a) WHAT.** `crates/otter-vm/src/cache_ir.rs`. The op enum `CacheOp`
(`cache_ir.rs:45`) — `GuardShapeId` (`:46`), `LoadPrototype` (`:55`), `GuardKey`
(`:63`), `LoadDataSlotResult` (`:69`), `HasDataSlot` (`:77`), `StoreDataSlot`
(`:85`), `StoreAddTransition` (`:92`) — and `CacheStub` (`cache_ir.rs:100`) with
its index-referenced tables `shape_ids` (`:104`), `slot_hits` (`:108`), `keys`
(`:109`), `transitions` (`:115`), operand file `[receiver=0, prototype=1]`. Three
freezes:
1. Treat the `CacheOp` stream + table encoding as a **versioned ABI** with a
   stamped version constant; changing op semantics bumps the version.
2. Add a **copy-on-compile snapshot** type — an immutable view of a site's
   `CacheStub` tables — so the optimizing tier observes a stable shape set and
   field offsets for the duration of a compile (it must not read live mutable
   tables).
3. **Feedback-vector container shape.** Define the per-callsite feedback slot as a
   stable record `{ state: Uninit|Mono|Poly|Megamorphic, stub: CacheStub,
   call_target_feedback }`, and bring the separate `MethodCallFeedback`
   (`Poly`/`Megamorphic`) and the native-fn-identity Array/Collection
   builtin-fast-dispatch under one indexable per-site container so the JIT has a
   single feedback surface to index. (Whether to fold builtin dispatch *into*
   CacheIR is deferred — see B4.)

**(b) WHY.** SpiderMonkey's Warp transpiles the recorded CacheIR op stream
straight to Ion MIR (`WarpCacheIRTranspiler`), baking stub-field shapes/offsets;
V8's JSHeapBroker freezes a `ProcessedFeedback`/`PropertyAccessInfo` snapshot
before lowering. Otter's `CacheStub` is already the JSC/SM-grade single IC
representation — the gap is the snapshot discipline and a stable container shape
the JIT can index. Shapes are already interned/immortal/pinned (the soundness
precondition both V8 and SM require for a baked shape guard).

**(c) EFFORT.** M.

**(d) RISK.** Deopt/correctness. `StoreAddTransition` (`cache_ir.rs:92`) must
record the *exact* target shape so a transpiled store deopts correctly when the
guard fails; a drifted op semantic silently miscompiles every site of that
opcode. The snapshot must be GC-safe (it references pinned shapes, so it holds no
movable `Gc`).

**(e) HOW IT SETS UP THE JIT.** The optimizing tier lowers each `CacheOp` to IR
(`GuardShapeId`→shape-check, `LoadDataSlotResult`→`load [base+slot*4]`,
`StoreAddTransition`→guarded transition + slab grow) by baking the snapshot's
shape ids and slot offsets — transpiling feedback instead of re-running ICs.

---

### A5 — Add monotone per-slot *representation* to the shape contract

**(a) WHAT.** `object.rs` shape modules (`shape_body.rs`, `shape_runtime.rs`).
The shape (`ShapeHandle`, `object.rs:497`) already carries slot kind/flags for
fast objects (per the module docs at `object.rs:506–508`); add a per-slot
**representation** tag `{ Tagged, Int32, Double }` with a **monotone widening**
transition (`Int32 → Double → Tagged`, never narrowing) recorded on the shape,
and an invalidation hook when a write violates the cached representation.

**(b) WHY.** V8 tracks field representation per descriptor (`Smi/Double/HeapObject/Tagged`)
and keeps the transition monotone + guarded so the optimizing tier holds values
unboxed in registers across a loop and deopts on violation; SpiderMonkey's
DescriptorArray and JSC's `PropertyOffset`/inferred type do the same. Otter's own
memory notes the next lever is repr-selection with loop-carried unboxed residency
— that depends on the *VM* exposing per-shape field representation the same
monotone way, which does not exist today.

**(c) EFFORT.** M–L.

**(d) RISK.** Deopt/correctness. The representation guard is a deopt point: a
store that widens a slot must invalidate dependent compiled code (or the slot's
representation must be re-read and deopt taken). Interacts with the
`CompressedValue` `0b010` boxed-HeapNumber tag (`value/compressed.rs:34`) — a
`Double`-repr slot may still be physically a boxed cell, so the repr tag and the
slot tag must agree. GC: shapes are pinned/immortal so no rooting hazard.

**(e) HOW IT SETS UP THE JIT.** The JIT bakes `CheckShapeId` then an *unboxed*
load at a known representation, and keeps a loop-carried double in an FP register
with a deopt-on-widen guard — impossible without monotone per-slot repr in the
VM.

---

### A6 — Freeze `Value`/`CompressedValue` encoding constants as a baked, asserted layout

**(a) WHAT.** `crates/otter-vm/src/value/tag.rs` and `value/compressed.rs`.
Register `Value`: `NUMBER_TAG = 0xfffe_0000_0000_0000` (`tag.rs:50`),
`OTHER_TAG = 0x2` (`tag.rs:54`), `DOUBLE_ENCODE_OFFSET = 2^49` (`tag.rs:64`),
`NOT_CELL_MASK` (`tag.rs:67`), `is_cell_bits` (`tag.rs:117`). Slot
`CompressedValue` (`#[repr(transparent)] u32`, `compressed.rs:64`): tags
`TAG_BOXED=0b010` (`:34`), `TAG_IMMEDIATE=0b100` (`:36`),
`TAG_FUNCTION_ID=0b110` (`:38`), `TAG_MASK=0b111` (`:40`), cell-ref `0b000`;
`is_gc_offset` = tag in `{0b000, 0b010}` (`:76–81`); `compress`/`decompress`
(`:110`/`:152`). Introduce a `JitValueLayout` struct of these constants and
`debug_assert!` the baked values against it — mirroring the existing
`JitGcBarrierLayout` discipline. Do **not** change the encoding (the dual
8-byte-register / 4-byte-slot ABI is correct and matches JSC/V8; see the
cross-engine verdict). Just freeze it.

**(b) WHY.** JSC bakes `NumberTag`/`DoubleEncodeOffset` as compile-time literals
into JITted box/unbox; V8 bakes the cage base and Smi shift. Otter already
debug-asserts the baked GC-barrier layout bits — extend that exact discipline to
the value/object constants so a frozen JIT and the interpreter can never diverge.

**(c) EFFORT.** S.

**(d) RISK.** Deopt. This is the sharper-for-Otter point from the V8/JSC
synthesis: the deopt frame-state (A7) must record *which of the four
`CompressedValue` tags* a slot holds — a verbatim cell offset (`0b000`), a boxed
HeapNumber (`0b010`), an immediate (`0b100`), or a function id (`0b110`) — and
how to reconstitute a full 8-byte `Value` from the 4-byte slot. Freezing the tag
constants now is what makes that deopt record well-defined.

**(e) HOW IT SETS UP THE JIT.** Inline box/unbox and the single-shift decompress
(`decompress`, `compressed.rs:152`) are baked from frozen constants; the
pointer-cheap `top16==0` verbatim deref (`tag.rs:41`) lets the JIT emit a free
cell load with no mask.

---

### A7 — Define the exact-PC deopt frame-state record + safepoint stack-map ABI

**(a) WHAT.** New VM ABI over the flat per-isolate `Value` stack with windowed
frames (P2). Two records, defined now, populated by the future JIT:
1. **Frame-state table**, keyed by **byte PC**, mapping each interpreter
   register / window slot → `{ location (register | stack-slot | constant),
   representation }`, where representation distinguishes `Tagged` / `Int32` /
   `Float64` / **which `CompressedValue` tag** (per A6) so a boxed value can be
   re-materialized on deopt.
2. **Safepoint stack maps**, one per GC-safe point (every call and allocation
   site), marking which window slots / spill slots hold tagged (rootable)
   pointers, so the **moving** collector finds and *updates* JIT-held roots
   without conservative scanning.

**(b) WHY.** This is the load-bearing contract that lets a moving collector
coexist with an optimizing tier. V8: `TranslationArray` (per-bytecode-offset
value location + representation, used by `Deoptimizer` to rematerialize) +
`SafepointTable`. SpiderMonkey: `Snapshots`/`RResumePoint` rebuild a
`BaselineFrame` at an exact bytecode PC. JSC: `ValueRecovery` map + bytecode
index. Otter has **none** of this today — it is the largest gap and must be
designed into the VM *before* JIT re-enable, not retrofitted.

**(c) EFFORT.** L.

**(d) RISK.** **Highest in the document — moving-GC rooting *and* exact-PC
deopt, the two halves of the correctness bar.** A missing or wrong stack-map
entry = a moving collector corrupts a live JIT root; a wrong frame-state entry =
deopt reconstructs a garbage interpreter frame. Both are silent and catastrophic.
The frame-state record must survive every layout change in A1/A2/A6/A8.

**(e) HOW IT SETS UP THE JIT.** Every guard failure, lazy deopt, and loop-OSR
entry reconstructs the exact interpreter frame at the right PC; the GC updates
spilled roots in optimized frames precisely. This is the contract the JIT lowers
*into*, and the reason it must be frozen first.

---

### A8 — Freeze the windowed frame/stack header layout

**(a) WHAT.** The P2 flat register-stack frame: fix the frame-header field
offsets (caller-window save, saved byte PC, callee, argument count, `this`),
the `this`/argument/local virtual-register numbering, and the
register-window-base relationship, as a stable, documented ABI. Keep `VmError`
`Copy`/`≤24B` with the payload in a per-isolate slot (P3 thin error) so the frame
stays a pure value array — exception state lives off the hot frame path, as in
JSC's `VM::m_exception`.

**(b) WHY.** JSC's baseline/DFG addresses every virtual register as a constant
displacement off the frame pointer and relies on fixed frame-header offsets; V8's
Ignition register file has stable per-function register indexing; Hermes
`StackFrameLayout` and BeamAsm's baked PCB offsets (`htop`/`stop`/`hend`) are the
same pattern. The JIT does not need a *specific* physical layout — it needs a
*deterministic, recorded* one.

**(c) EFFORT.** M.

**(d) RISK.** Deopt. The frame-state record (A7) references these slots by index,
so slot assignment must be deterministic per PC; this is why A8 lands before A7.
GC: stack slots are scanned directly as roots (`root_slots`/`external_visit` in
`scavenger.rs:134–164`) — keep that direct-scan model (Hermes
`PinnedHermesValue`, BEAM register stack), do not introduce pervasive handles.

**(e) HOW IT SETS UP THE JIT.** The JIT bakes constant displacements for
locals/args and a fixed prologue; deopt lands at the correct slots.

---

### A9 — Freeze `GcHeader` flag-bit assignment + the inline write-barrier ABI face

**(a) WHAT.** `crates/otter-gc/src/header.rs`: `FLAG_YOUNG=0b0000_0100`
(`header.rs:38`, re-exported `GENERATION_YOUNG_FLAG` `:48`),
`FLAG_FORWARDED=0b0000_1000` (`:49`), `FLAG_PINNED=0b0001_0000` (`:50`),
`FLAG_SWEPT=0b0010_0000` (`:59`), mark-color bits, all in the `AtomicU8` `flags`
of `GcHeader { type_tag:u8, flags:AtomicU8, size_bytes:u32 }` (`header.rs:75–79`).
And `crates/otter-gc/src/barrier.rs::write_barrier` (`barrier.rs:62`), whose
generational arm marks the parent header's card via `mark_card(byte_offset)`
(`barrier.rs:84`, `page.rs:182`). Freeze the **bit assignment** and the
**barrier insert sequence** as the bakeable ABI face. The *mechanism* change
(card table → precise object-/slot-granular set, and whatever new
`FLAG_REMEMBERED`-style bit it needs) is owned by `VM_GC_REDESIGN.md`; this entry
only requires that whatever the GC doc lands becomes a **frozen, asserted bit
layout + a fixed 2–3 instruction insert sequence** before the JIT re-enables —
extend the existing `JitGcBarrierLayout` debug-assert to cover the final bits.

**(b) WHY.** V8 inlines `RecordWriteField` (host-old ∧ value-young → SlotSet
insert); JSC inlines `load cellState; cmp PossiblyBlack; branch slow`; BeamAsm
inlines bump-alloc + a cold GC call. The future Otter JIT must emit the barrier
inline — "no per-call bridge stubs" — which means the remembered-set
representation and its insert sequence must be a frozen, compile-time-constant
ABI before the JIT bakes it.

**(c) EFFORT.** S–M (the ABI freeze; the GC mechanism is costed in
`VM_GC_REDESIGN.md`).

**(d) RISK.** GC. `flags` is `AtomicU8` because the marker and mutator race on
mark transitions (`header.rs:20–21`); any new remembered bit must respect that
atomicity. The `is_swept` guard in the current dirty-card walk
(`scavenger.rs:518`, `scavenger.rs:557`) is the corpse-walk workaround whose fate
is decided by the GC redesign — note it here only so the JIT's barrier ABI does
not bake a soon-dead card formula.

**(e) HOW IT SETS UP THE JIT.** The JIT later emits the generational barrier as a
fixed inline sequence (load parent flags, test the old/remembered bit, cold-call
the slow path) from the frozen bit layout — replacing card-address arithmetic
with a bakeable contract.

---

# Section B — Do WITH JIT (list now, defer)

These only pay off once an optimizing tier exists. They are recorded so the
BEFORE-work does not accidentally foreclose them, but they are **not** done now.

| # | Deferred change | Pays off when | Engine lesson |
|---|---|---|---|
| B1 | Per-shape variable inline-slot count (drop fixed `INLINE_SLOT_CAP=4`) | JIT bakes per-shape offsets | V8 slack tracking, JSC `inlineCapacity`, Hermes `DIRECT_PROPERTY_SLOTS=5` |
| B2 | Move `jit_proto` onto the shape, reclaim 8B/object | JIT no longer needs per-object proto guard | QuickJS proto-on-shape, SM/JSC proto-on-structure |
| B3 | Megamorphic escape valve (global stub cache) for `CacheStub` | JIT lowers poly→megamorphic | V8 stub cache, JSC megamorphic |
| B4 | Fold Array/Collection builtin dispatch into CacheIR-guarded stubs | JIT wants one lowering surface | SM emits CacheIR for dense-element; V8 keeps it in typed-lowering |
| B5 | Unboxed-double inline slot (kill the `0b010` HeapNumber box) | repr-selection keeps doubles unboxed | JSC 8-byte inline slot |

**B1 — Per-shape inline-slot count.** `ObjectBody.inline_values` /
`INLINE_SLOT_CAP=4` (`object.rs:566`/`:574`). Today every object pays for 4 inline
slots; item4's measurement showed body 72→96B and RSS +3..+26% with no realized
payoff *because the interpreter reaches all slots uniformly through `values_ptr`*
— there is no interp win from per-shape sizing. The win is JIT-only (bake a
per-shape inline offset and skip the overflow indirection). **Defer**; revisit
alongside A1's size lock. Note the tension with QuickJS's "zero inline slots,
tiny body" RSS result — the right inline count is a JIT-era tuning decision, not
a pre-JIT one.

**B2 — `jit_proto` onto the shape.** `ObjectBody.jit_proto` @
`OBJECT_BODY_JIT_PROTO_OFFSET` (`object.rs:532`/`:648`) exists purely so the
method-inline guard reads the prototype from the body in one load. With no JIT,
it is 8 dead bytes per object that QuickJS/SM/JSC keep on the shared shape. **Do
not move it now** — removing the field pre-JIT is churn that A1 would have to
re-pin; weigh it when the JIT's method-inline guard is actually designed (it may
prefer chasing proto through the already-guarded shape).

**B3 — Megamorphic escape valve.** `CacheStub` (`cache_ir.rs:100`) currently runs
a linear operand-file program. V8/JSC degrade a megamorphic site to a hashed
global stub cache rather than a linear scan. Only matters once the JIT lowers
polymorphic guard chains. **Defer.**

**B4 — Builtin dispatch into CacheIR.** Otter keeps Array/Collection method ICs as
a separate native-fn-identity fast-dispatch, *outside* `CacheStub`. SM emits
CacheIR even for dense-element fast paths (one lowering surface); V8 instead
specializes arrays in typed-lowering, *not* the IC. Opinionated: **keep the split
for now** — it is idiomatic (JSC dispatches array intrinsics by
`indexingType`/callee identity too) and folding it in pre-JIT buys nothing.
Revisit when the JIT's lowering surface is concrete.

**B5 — Unboxed-double inline slot.** `CompressedValue` `0b010`
(`value/compressed.rs:34`) boxes any double/wide-int into a separate
`HeapNumber` cell, adding a load + a GC edge per numeric property — a cost JSC's
8-byte inline slot never pays. Changing this trades away the 32-bit cage
compression density and only pays once A5's repr-selection keeps doubles unboxed
in the JIT. **Defer**; if double-heavy property workloads regress in the
interpreter first, ensure `HeapNumber` allocation stays cheap and that storing an
immediate over a boxed slot is barrier-skipped (the existing smi-skip at store
sites already does this).

---

## Cross-links

- **`VM_GC_REDESIGN.md`** — remembered-set redesign, on-heap overflow-slab
  migration (A3 prerequisite), minor-GC scan rewrite, variable-size body
  support. The GC mechanism behind A3 and A9.
- **`VM_JIT_FRIENDLY_REFACTOR_PLAN.md`** — P2 flat register stack (A8), P3 thin
  `VmError` (A8), P4 remembered-set verdict (superseded by `VM_GC_REDESIGN.md`).
- **`VM_ABI_AUDIT.md`** — areas A–F audit this document acts on.
