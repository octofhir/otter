# VM_ENGINE_RESEARCH_V2 — Per-Engine Synthesis for the Otter VM ABI

> **Status: RESEARCH ONLY.** This document is diagnosis + comparison. It makes
> no decisions. The decisions live in the companion docs
> (`VM_JIT_FRIENDLY_REFACTOR_PLAN.md` and `VM_ABI_AUDIT.md`), which this doc
> references by name. Nothing here proposes a JIT edit — the JIT is frozen and
> off; every recommendation is framed as a *VM ABI* the future JIT will later
> lower against (stable bakeable offsets, a stable remembered-set contract, and
> exact-PC deopt/frame-state metadata).
>
> **The priority question** (asked of every engine, most depth here): *what is
> the minor-GC old→young remembered-set algorithm, and how does it survive a
> mutable heap with off-page property storage?* Otter today uses a card table +
> a full per-page object-header walk (`scan_old_dirty_cards`), which `P4` of
> `VM_JIT_FRIENDLY_REFACTOR_PLAN.md` judged "intractable to make precise"
> because traced slots routinely live in malloc storage outside any GC page.
> This document tests that verdict against seven engines.

---

## 0. Otter Ground Truth (the thing we are changing)

Every engine section below cites back to these exact structs/offsets. Verified
against the tree at the time of writing.

### 0.1 Value ABI

| Construct | Definition | Cite |
|---|---|---|
| `Value` | `#[repr(transparent)] u64`, JSC pointer-cheap NaN-box over a 4 GiB-aligned cage. Heap pointers stored **verbatim** (top16=0, free deref), doubles `+2^49`, int32 `NUMBER_TAG`, immediates `OTHER_TAG`. `size_of::<Value>()==8`. | `crates/otter-vm/src/value/mod.rs`, `value/tag.rs` |
| `CompressedValue` | `#[repr(transparent)] u32` **property-slot** value (NOT `Value`). Low-3-bit tag: `0b000` cell offset verbatim (8-aligned `RawGc`), `0b010` boxed `HeapNumber`, `0b100` immediate, `0b110` function-id (`id<<3`). `is_gc_offset()` = tag ∈ {000,010}. | `crates/otter-vm/src/value/compressed.rs:76` |
| `RawGc` | `u32` cage-relative offset (32-bit compressed pointer). `cage_base() \| offset` = full address; moving collector rewrites the 4-byte slot in place. `MAX_CAGE_SIZE_BYTES = 1<<32`, `CAGE_ALIGN_BYTES = 1<<32` (default cage 2 GiB). | `crates/otter-gc/src/compressed.rs:56,70` |

### 0.2 Object Model — `ObjectBody`, `#[repr(C)]`, **96 bytes**

Fields in declaration order: `shape: ShapeHandle` (hidden-class handle,
**`OBJECT_BODY_SHAPE_OFFSET == 0`**, statically asserted) → `values_ptr: *mut
CompressedValue` (cached slab base, `OBJECT_BODY_VALUES_PTR_OFFSET`, JIT bakes
it) → `values: Vec<CompressedValue>` (**out-of-line malloc overflow slab**) →
`dictionary_shape_id` → `shape_cache_mode` → `jit_proto: JsObject` (**prototype
ON THE OBJECT**, `OBJECT_BODY_JIT_PROTO_OFFSET`, read by the method-inline
guard) → `extensible: bool` → `slot_attrs_overridden: bool` → `exotic:
Option<Box<ExoticSlots>>` (~140B, `None` for plain objects/class instances) →
`inline_values: [CompressedValue; INLINE_SLOT_CAP]` (**in-body slab**,
`INLINE_SLOT_CAP == 4` — the "item4" result) → `slab_len: u16`.

- `values_ptr` points at `inline_values` when `slab_len <= 4` (**in-page**, in
  the GC body) else at the `values` Vec (**off-page malloc**).
  `refresh_values_ptr()` recomputes it after every move/grow/shrink
  (`object.rs:642`, `push_slot` at `object.rs:706`).
- Property read = shape-id guard ⇒ `decompress(values_ptr[slot])`. Slot meta
  (flags/kind) lives in the **shape** for fast shaped objects; `ExoticSlots`
  only for dictionary/attribute-overridden.
- `ObjectBody` is **fixed size per type**: `GcHeader.size_bytes ==
  sizeof(ObjectBody)+header`, set once at alloc. `SafeTraceable::trace_slots_safe`
  (`object.rs:~1086`) walks `slab_len` words from `values_ptr`, refreshing it
  first because `memcpy`-on-move leaves the cached base stale. **Variable-size
  bodies are NOT supported today.**
- item4 result: body 72→96B + cage; RSS +3..+26% (nbody 16→44MB anomalous),
  +6% richards time, precise `SlotSet` shelved. **No realized payoff yet.**

### 0.3 Inline Caches — `cache_ir::CacheStub` (`crates/otter-vm/src/cache_ir.rs`)

One representation for Load/Has/Store ICs (CacheIR-style). `enum CacheOp`
(`cache_ir.rs:45`): `GuardShapeId{obj,shape}` (`:47`), `LoadPrototype{obj,dst}`
(`:55`), `GuardKey{key}`, `LoadDataSlotResult{obj,hit}`, `HasDataSlot`,
`StoreDataSlot`, `StoreAddTransition` (`:92`). Operand 0 = receiver, operand 1 =
prototype after `LoadPrototype`. `CacheStub` (`:100`) carries
shape_ids/hits/slot_hits/keys/transitions tables; ops reference by index. Shapes
interned + immortal (non-moving old space), pinned. Monomorphic own-data fast
path bypasses the operand file. Array/Collection method ICs are **separate**
builtin-fast-dispatch keyed on native-fn identity (NOT CacheIR).

### 0.4 GC — moving generational, Cheney young gen

- Page: `PAGE_SIZE = 256 KiB` (`page.rs:60`), `CARD_SIZE = 512B` (`:64`),
  `CARDS_PER_PAGE = 512` (`:68`), card bitmap 64B/page (`CARD_BITMAP_BYTES`,
  `:71`). `mark_card(byte_offset)` sets bit `card/8 |= 1<<(card%8)`.
- `GcHeader` (`header.rs`): `type_tag:u8`, `flags:AtomicU8`
  (`FLAG_YOUNG = 0b100` at `:38`, plus mark color), `size_bytes:u32`.
  `is_young/is_old/is_forwarded/is_swept`.
- **Write barrier** (`barrier.rs:62` `write_barrier`): smi-skip at the call
  site, then `if !child.is_null() && parent.is_old()` (`:76`) `&& child.is_young()`
  (`:79`) ⇒ `page_header.mark_card(byte_offset)` (`:84`) where `byte_offset`
  is the **PARENT HEADER's** page offset — never the mutated slot — *because
  traced slots routinely live in malloc side storage (off-page `values` Vec,
  `exotic` Box, shape handles) outside any heap page; masking such a slot to
  "its page" fabricates a wild page header.* ~78 `record_write` call sites.
- **Minor GC** (`scavenger.rs:129` `scavenge`): (1) root_slots, (2)
  external_visit roots, (3) `scan_old_dirty_cards`, (4) `cheney_scan` to
  convergence, (5) ephemeron fixpoint, (6) weak registry, (7) bump
  `survival_age`, (8) flip from↔to. `PROMOTE_AFTER_SURVIVALS = 1`.
  - `scan_old_dirty_cards` (`:498`): for each old page with ANY dirty card,
    snapshot dirty offsets, clear bits, then **walk EVERY header**
    `PAGE_HEADER_SIZE..bump_cursor` (O(objects/page)); body intersect-test vs
    dirty cards; if overlap && `!is_swept` ⇒ `trace_one` (**re-trace WHOLE
    object**). No object-start table; no slot-granular scan.
  - `process_slot` (`:217`): evac young child, rewrite the 4-byte slot in
    place, then `remember_parent_card_for_young_child` (`:259`) re-marks the
    PARENT card if evac minted an old→young edge.
  - `cheney_scan` (`:575`): separate to-space + freshly-promoted scan.
- `P4` of `VM_JIT_FRIENDLY_REFACTOR_PLAN.md` judged a precise remembered set
  **intractable** due to the off-page-slot wall.

### 0.5 Frames / Errors / JIT

Flat per-isolate Value stack, windowed frames (P2). `VmError` `Copy` ≤24B,
payload in a per-isolate slot (P3 thin error). JIT: **frozen, off by default,
fully stale** — bakes the pre-rewrite Value/slab ABI; re-enable is the *next*
project. Do not propose JIT edits; propose the VM ABI it will later bake.

**Correctness bar for every proposal below:** moving-GC invariants + manual
rooting + exact-PC deopt frame-state metadata must survive.

---

## 1. V8 (Orinoco scavenger + MinorMC; Oilpan is a separate C++ GC, not covered)

### 1.1 Value ABI
V8 uses **low-bit tagging**, not NaN-boxing: Smi = low bit 0 (`value<<1`),
HeapObject = low bit 1 (`kHeapObjectTag`). With pointer compression every
in-heap tagged field is a 32-bit `Tagged_t` decompressed against a 4 GiB-aligned
cage base (`kPtrComprCageBaseAlignment == 4GB`). This is **one-for-one with
Otter's `RawGc` u32 cage-relative offset** (`compressed.rs:70`,
`CAGE_ALIGN_BYTES = 1<<32`) and with Otter's `CompressedValue` u32 in-slot
encoding. The decisive divergence: V8 compresses *every* on-heap tagged slot to
`Tagged_t` (in-object fields, FixedArray elements, all of it), but reuses that
same 32-bit form in registers and decompresses on demand. Otter splits: the
mutator-facing `Value` is a fat 8-byte JSC NaN-box (`value/mod.rs`,
top16=0 verbatim deref) while the in-heap slot is the separate 4-byte
`CompressedValue` (`value/compressed.rs:76`). So **Otter already matches V8's
slot-level compression** (`CompressedValue` ≡ `Tagged_t`) and additionally
carries a register-only fat Value the JIT will bake. V8 does **not** NaN-box
doubles: a heap double is a boxed `HeapNumber` (own tagged pointer) — exactly
Otter's `CompressedValue` tag `0b010` boxed `HeapNumber` — while Maglev/Turboshaft
carry doubles **unboxed** in FP registers as a `Float64` representation and
re-box only at representation boundaries / deopt. Lesson: V8 proves a JIT needs
a stable *in-heap* encoding (which Otter has) plus per-value representation
tracking; the on-stack NaN-box `Value` is convenience the optimizing tier mostly
bypasses.

### 1.2 Object / property storage
V8 `JSObject` is the template Otter approximates. `Map` pointer at offset 0
(`kMapOffset == 0`) ≡ Otter `OBJECT_BODY_SHAPE_OFFSET == 0` — both are the
single guard word the JIT bakes (V8 `CheckMaps`, Otter `GuardShapeId`). Then a
`properties` field, an `elements` field, then **in-object properties** inlined
at `kHeaderSize + index*kTaggedSize` ≡ Otter `inline_values[4]`. Instance size
is fixed per `Map` (`Map::instance_size`) ≡ Otter `GcHeader.size_bytes ==
sizeof(ObjectBody)+header`; both forbid variable-size bodies. Per-field meta
(kind/attributes/representation/field-index) lives in the Map's
`DescriptorArray` ≡ Otter keeping slot meta in the **shape**, spilling to
`ExoticSlots` only for dictionary/overridden.

**The one structural divergence that drives everything in §1.3:** when V8 spills
past in-object capacity, the overflow `PropertyArray` (and indexed `FixedArray`
`elements`) is **itself a managed HeapObject on a GC page**, not malloc side
storage. Otter's overflow is `values: Vec<CompressedValue>` — a **malloc buffer
outside the cage** — reached through `values_ptr` (`OBJECT_BODY_VALUES_PTR_OFFSET`),
rebased by `refresh_values_ptr()`. V8 has no `values_ptr` indirection because the
backing store is a normal tagged field moved/updated by GC like any pointer. V8
also reclaims over-allocated in-object slots via slack tracking
(`Map::CompleteInobjectSlackTracking`) — a refinement Otter lacks (`INLINE_SLOT_CAP`
is a flat 4). V8 keeps the prototype on the shared `Map` (`Map::prototype`);
Otter denormalizes it onto the body (`jit_proto`,
`OBJECT_BODY_JIT_PROTO_OFFSET`) for a one-load method-inline guard.

### 1.3 Write barrier + remembered set + minor GC — **the priority answer**
V8 uses a **slot-precise** OLD_TO_NEW remembered set, **not** a card table, and
the minor GC **never walks page headers**.

1. **Barrier** (`heap/heap-write-barrier`, generated `RecordWrite`): on a tagged
   store the inline fast path reads page flags off the host's `MemoryChunk`
   (`kPointersFromHereAreInterestingMask` = host old/interesting,
   `kPointersToHereAreInterestingMask` = value points to new space). If both,
   `RememberedSet<OLD_TO_NEW>::Insert(host_chunk, slot_address)` records the
   **exact slot address**. Contrast `barrier.rs:84` where Otter records the
   **parent header's** card and discards which slot was written.
2. **Set representation** (`heap/slot-set.h`): per-page array of *buckets*; each
   bucket is a bitmap with **one bit per `kTaggedSize`-aligned slot**.
   `Insert` = `(bucket, bit)` from the slot's page offset. **No object-start
   table is needed** because the set stores slot addresses directly — you never
   have to find the enclosing object.
3. **Minor GC** (`heap/scavenger.cc`): `RememberedSet<OLD_TO_NEW>::Iterate`
   visits **only set bits**, handing each slot to `ScavengeObject`, which
   evacuates the young child and rewrites that one 4-byte slot — O(dirty slots),
   never O(objects/page), never a whole-object re-trace. The callback returns
   `REMOVE_SLOT` (target got promoted out of new space) or `KEEP_SLOT` (still
   young), **pruning the set inline** — V8's replacement for Otter's
   `remember_parent_card_for_young_child` re-dirty. Then a Cheney closure
   (`Scavenger::Process`) drains only freshly copied/promoted objects. **One
   precise pass + Cheney; no separate card-retrace pass, no re-dirty loop.**

**The off-page-slot wall — V8's dodge.** V8 also has overflow (`PropertyArray`)
and indexed (`FixedArray`) backing stores, but those are **first-class
HeapObjects on `MemoryChunk` pages**. A store into a `PropertyArray` slot
records that slot in the *PropertyArray's own* page slot set — precise, because
the slot address is page-resident. **There is never a malloc-owned traversable
slot.** Otter's wall (`barrier.rs` comment: parent-card recording because slots
live in malloc storage) is **self-inflicted by storing overflow `values`/`exotic`
in Rust malloc.** V8's answer to "in-object everywhere?" is explicitly **no** —
keep the spill array, but make the spill array a *heap object*. This is the
single change that makes Otter's shelved precise `SlotSet` tractable: the
blocker is the storage model (malloc Vec), **not** the remembered-set algorithm.

**Maps explicitly onto Otter's three documented wastes:** (a) the
O(objects/page) header walk in `scan_old_dirty_cards:498` — V8 has no header
walk; the SlotSet *is* the object-start replacement; (b) whole-object re-trace
per dirty card — V8 scavenges exactly the recorded slot; (c) the
re-dirty/double-pass — V8 prunes via `REMOVE_SLOT`/`KEEP_SLOT`.

### 1.4 Frame / stack ABI
Ignition: a fixed frame header (function, context, bytecode array, bytecode
offset, feedback vector) plus an interpreter **register file** as stable
`[fp - k]` slots; accumulator in a dedicated register. Register indices have
stable per-function meaning; bytecode offsets are resumption points. Maglev /
Turboshaft bridge back through `TranslationArray`/`FrameTranslation` deopt
metadata recording, per interpreter register, **where** the value lives
(register/stack/constant) **and its representation** (Tagged/Int32/Float64/
HeapNumber), so the deoptimizer rematerializes (re-box Float64→HeapNumber,
re-tag Int32→Smi) at an exact bytecode PC. Safepoint stack maps
(`SafepointTableBuilder`) mark tagged spill slots so the moving GC updates
spilled roots. This is precisely the contract Otter's flat windowed stack (P2)
must freeze: stable interpreter-register indexing + per-deopt-point
{location, representation} + safepoint stack maps over the flat Value stack.

### 1.5 IC contract
V8 stores per-site feedback in a `FeedbackVector` slot ({map, handler}, or a
polymorphic array). Handler = a Smi-encoded `LoadHandler`/`StoreHandler`
(bit-packed kind/inobject/field-index/representation + proto-check count) or a
`DataHandler` HeapObject carrying a validity cell + holder. Maps directly onto
Otter `CacheStub`: `GuardShapeId` ≡ `CheckMaps`; `LoadDataSlotResult`/`HasDataSlot`
≡ field-index handler; `StoreDataSlot` ≡ store handler; `StoreAddTransition`
(`cache_ir.rs:92`) ≡ transitioning-store handler; `LoadPrototype` (`:55`) ≡
the proto-chain holder/validity walk. The monomorphic own-data fast path that
bypasses Otter's operand file ≡ V8's inlined monomorphic Smi handler. The one
thing Otter should keep in mind: V8 has a **megamorphic escape valve** (a global
stub-cache hash on map+name) so a site that goes megamorphic degrades to hashed
dispatch instead of a linear operand scan. V8 keeps maps **weak** in feedback
(dead hidden classes die); Otter interns shapes **immortal/pinned** — simpler,
never reclaims dead shapes. Maglev does not read the raw nexus during graph
build: `JSHeapBroker` snapshots it into an immutable `ProcessedFeedback` /
`PropertyAccessInfo` — the **snapshot discipline** Otter must add for JIT
consumption.

### 1.6 How the JIT consumes the VM ABI
Stable bakeable contract: (1) Map@0 → single-compare guard ≡ Otter
`OBJECT_BODY_SHAPE_OFFSET == 0`; (2) in-object field offset is a pure function
of the Map → JIT bakes the byte offset ≡ Otter `inline_values` base + slot
index, with the caveat that Otter additionally exposes `values_ptr`
(`OBJECT_BODY_VALUES_PTR_OFFSET`) which the JIT bakes and the VM must keep
current after every move/grow; (3) fixed instance size; (4) immutable feedback
snapshot (`ProcessedFeedback`) — Otter's `CacheStub` tables must be
copy-on-compile; (5) per-PC frame-state translation {location, representation};
(6) safepoint stack maps. V8 bakes the generational barrier inline
(`RecordWriteField`: value-in-new + host-in-old → SlotSet insert) — Otter's JIT
will likewise inline its barrier, so the remembered-set representation (card
today, ideally a SlotSet) must be a frozen, bakeable, compile-time-constant
layout (`JitGcBarrierLayout` discipline already exists).

---

## 2. JavaScriptCore (WebKit) — Riptide concurrent generational mark-sweep

### 2.1 Value ABI
JSC `JSValue` on 64-bit is the **literal template Otter copied**: `u64` NaN-box,
cells stored **verbatim** (top16=0, free deref), doubles `+ DoubleEncodeOffset
(1<<49)`, int32/immediates via `NumberTag = 0xfffe000000000000` / `OtherTag` /
`BoolTag`. `isCell()` ≡ `(bits & (NumberTag|OtherTag)) == 0`. Otter's `Value`
(`value/mod.rs`, `value/tag.rs`) is essentially ABI-identical. **The divergence
that matters:** JSC has **no 32-bit compressed property slot** — every JSObject
inline slot and every Butterfly slot is a full 64-bit `WriteBarrier<Unknown>`
(8 bytes), so doubles live **inline** with zero indirection. Otter's
`CompressedValue` (`compressed.rs:76`) halves slot footprint but pays a
`HeapNumber` box+deref for tag `0b010` (any double / wide int) — a cost JSC never
pays. Net: Otter is more cache-dense in property storage but adds a boxing
indirection; **if double-heavy property workloads regress, the `0b010` box is the
suspect** and JSC's 8-byte inline slot is the documented alternative. JSC pointers
in `JSValue` are full 48-bit because **JSC does not move cells and does not
compress** (see §2.3); Otter's `RawGc` u32 compression is justified *only* by
moving.

### 2.2 Object / property storage
A direct port. (1) **Inline storage**: N inline slots after the cell header,
N = `Structure::inlineCapacity()` (default 6) ≡ Otter `inline_values[4]`. (2)
**Out-of-line**: the **Butterfly** — a separate allocation in the JSValueGigacage
— holds out-of-line named props (growing left, negative offsets) and indexed
elements (growing right) ≡ Otter `values: Vec<CompressedValue>` reached via
`values_ptr`. Otter `values_ptr`→`inline_values` when `slab_len<=4` else →Vec ≡
JSC reading inline vs `butterfly()->propertyStorage()`. (3) **Hidden class**:
`Structure` ≡ Otter `ShapeHandle` (`OBJECT_BODY_SHAPE_OFFSET==0`; JSC bakes
`CheckStructure`); owns the `PropertyTable` (name→`PropertyOffset`), transitions,
and `inlineCapacity`; `offset < firstOutOfLineOffset` ⇒ inline ≡ Otter resolving
slot from shape then `values_ptr[slot]`. JSObject is fixed-size (size from the
Structure's class); **all** variable growth goes to the Butterfly ≡ Otter's
fixed-size `ObjectBody`. JSC keeps the prototype on the **Structure**
(`Structure::storedPrototype`) — a structure check subsumes a prototype check —
whereas Otter puts it on the body (`jit_proto`) and must guard it separately.

### 2.3 Write barrier + remembered set + minor GC — **the priority answer**
JSC's eden (minor) collection **does not walk pages and uses no card table and
no object-start table**. Its remembered set is a **precise, object-granular
list of mutated parent cells**, maintained entirely by the barrier.

- **Barrier (CellState machine, `Heap::writeBarrier`):** each cell header
  carries `CellState ∈ {PossiblyBlack, DefinitelyWhite, DefinitelyGrey}`. After
  a full GC every live cell is `PossiblyBlack` (≡ "old"). Fast path is a single
  byte-compare on the **parent**: `if (from->cellState() == PossiblyBlack)
  writeBarrierSlowPath(from);` — **no child load, no page math, no card index.**
  The slow path appends `from` to a `WriteBarrierBuffer` (batched into the
  remembered set), deduped by the cell's own state bit. The remembered set is
  literally a list of mutated old parents. Eden GC treats exactly those as
  roots, drains them through the `SlotVisitor`, and stops — **O(remembered
  parents), not O(objects on any page).**
- **The off-page-slot wall — JSC has Otter's exact problem and dodges it the
  way Otter already leans, only precisely.** A store into an out-of-line
  property or array element writes into the **Butterfly** — a *separate*
  allocation from the owner cell, **JSC's exact analog of Otter's malloc
  `values` Vec**. JSC's barrier owner is the **object, never the butterfly**:
  `locationForOffset(offset)->set(vm, this, value)` passes `this` (the
  JSObject) as owner, so the barrier remembers the **owner cell**, not the
  butterfly slot. When eden re-visits the owner, `JSObject::visitChildren →
  visitButterfly` re-walks the entire out-of-line region. **This is precisely
  Otter's design intent** (`barrier.rs`: record the parent header, never the
  off-page slot; `trace_slots_safe` re-walks `slab_len` words including the
  off-page Vec). Otter and JSC made the **same correctness decision**; the
  *entire* performance gap is the **representation of "which parents"**:
  - JSC: a precise list of parent cell pointers. Eden visits each directly.
  - Otter: a **card bit** on the parent's page, and `scan_old_dirty_cards:498`
    **reconstructs** the parents by walking every header
    `PAGE_HEADER_SIZE..bump_cursor` and intersect-testing each body — Otter
    rebuilding the object-start information JSC never needed.
  So Otter pays **both** a find-cost (page walk) JSC doesn't have **and** a
  coarse whole-object re-trace (which JSC also accepts, from a precise list).
- **The double-pass / re-dirty is a tax of MOVING that JSC structurally
  lacks.** JSC's "generational" is **sticky mark bits**, no nursery copy:
  `MarkedBlock` has a mark bitmap + a `newlyAllocated` bitmap; eden GC does not
  clear marks (old cells stay black, skipped), scans only `newlyAllocated` +
  remembered roots, then folds `newlyAllocated` into the mark bitmap (logical
  promotion, **zero data movement**). Cells never move, so a slot is never
  rewritten, an old→young edge is either already logged or promoted in place,
  and there is no to-space to Cheney-scan. Otter's `process_slot:217` rewrite +
  `remember_parent_card_for_young_child:259` re-dirty + separate `cheney_scan`
  exist **only** because Otter copies/ages a young gen.
- **Historical note directly relevant to Otter:** pre-2017 JSC stored
  butterflies in a *copying* `CopiedSpace` and needed a `CopyBarrier<T>` so the
  mutator couldn't read a butterfly mid-relocation. JSC **deleted both** when
  Riptide went non-moving, to enable concurrent marking and kill the copy
  barrier. **Otter is doing exactly what JSC abandoned** (Cheney copying + in-
  place slot rewrite + `RawGc` compression that pays off only because it moves).
  This is a legitimate *different bet* — 32-bit compressed slots need a moving
  collector to be compactable — but it is **why** Otter has the wall, the
  re-dirty, and the find-cost JSC lacks.

**JSC refutes `P4`'s "intractable" verdict for the OBJECT-GRANULAR design.**
JSC is the existence proof that a precise remembered set coexists with off-page
(butterfly) storage: you remember the **in-page owner** (which has a header and
a card-able page), not the off-page slot. `P4` is true only for the
**slot-granular** variant (V8/SM store buffer), which is what hits Otter's wall.

### 2.4 Frame / stack ABI
A contiguous JS stack of `Register` slots (8-byte union of `EncodedJSValue`/
pointer/int) addressed via `CallFrame`. Fixed frame header (`CallerFrame`,
`ReturnPC`, `CodeBlock`, `Callee`, `ArgumentCount`), then `this`/args/locals at
virtual-register offsets from fp. Baseline/DFG address every VReg as a constant
fp displacement — the model Otter's P2 flat windowed stack converges on. Deopt
(OSR exit) carries a `ValueRecovery` map per VReg (register/stack/constant +
boxed/unboxed format) + exact bytecode index; FTL adds `B3::StackmapValue`.
**Sharper for Otter:** the recovery map must record whether a slot is a verbatim
cell offset (`0b000`), boxed `HeapNumber` (`0b010`), immediate (`0b100`), or
function-id (`0b110`), and how to reconstitute a full 8-byte `Value` from the
4-byte slot. Exception state lives in `VM::m_exception` off the hot frame path ≡
Otter `VmError` Copy ≤24B in a per-isolate slot (P3).

### 2.5 IC contract
JSC does **not** use CacheIR (that is SpiderMonkey; the lineage Otter named
`cache_ir` after). JSC's IC is `PolymorphicAccess` holding `AccessCase` objects,
per-site state in `StructureStubInfo`. Each `AccessCase` = {type (Load/Miss/
Replace/Transition/Getter/...), guarding `Structure*`, `PropertyOffset`,
`ObjectPropertyConditionSet` (proto-chain structure guards)}. `regenerate`
compiles the case list to spliced machine code. Maps onto `CacheStub`:
`GuardShapeId` ≡ per-case `Structure` identity guard; `LoadPrototype` + a second
`GuardShapeId` ≡ one element of the `ObjectPropertyConditionSet`;
`LoadDataSlotResult`/`HasDataSlot`/`StoreDataSlot` ≡ `Load`/`Miss`/`Replace`
with a `PropertyOffset` (inline vs out-of-line via `offset <
firstOutOfLineOffset` ≡ Otter inline_values vs Vec); `StoreAddTransition` ≡
`Transition` (JSC also reallocates the butterfly ≡ Otter grows the slab +
`refresh_values_ptr`). JSC interns Structures immortally so a baked `Structure*`
stays valid ≡ Otter shapes interned/immortal/pinned. Array/typed-array intrinsics
dispatch via `Structure::indexingType` + thunk generators keyed on callee
identity, **outside** the property stub ≡ Otter's separate native-fn-identity
builtin dispatch — corroborated as a sound boundary.

### 2.6 How the JIT consumes the VM ABI
(1) Stable cell offsets: `structureID`@0 (`CheckStructure` = load-compare at a
fixed offset), `butterflyOffset()` fixed ≡ Otter `OBJECT_BODY_SHAPE_OFFSET==0`,
`OBJECT_BODY_VALUES_PTR_OFFSET`, `OBJECT_BODY_JIT_PROTO_OFFSET` — must stay
byte-stable across any body refactor (the frozen JIT is stale precisely because
it bakes the *pre-rewrite* slab ABI). (2) Structure immortality ≡ pinned shapes.
(3) IC contract = `StructureStubInfo`/`AccessCase` layout ≡ Otter's `CacheStub`
operand/table contract. (4) Deopt = `ValueRecovery` map + exact bytecode index ≡
Otter's exact-PC frame-state, sharpened by `CompressedValue` tag reconstitution.
(5) Value ABI constants `NumberTag`/`DoubleEncodeOffset` baked as literals ≡
Otter's `NUMBER_TAG`/`OTHER_TAG`/`2^49` + low-3-bit tag constants, `debug_assert`'d
against baked values. (6) Barrier: JSC inlines `load cellState; cmp PossiblyBlack;
branch slow` — if Otter moves to an object-granular set, the bakeable barrier
becomes `load parent flags; test FLAG_REMEMBERED; if old∧unlogged → slow push`,
replacing card-address arithmetic; freeze a `FLAG_REMEMBERED` bit next to
`FLAG_YOUNG` (`header.rs:38`).

---

## 3. SpiderMonkey (Mozilla) — nursery copy + StoreBuffer; CacheIR

### 3.1 Value ABI
SM `JS::Value` on 64-bit is **punbox64**: a NaN-box where doubles are verbatim
and every non-double carries a 17-bit tag in the high NaN bits, payload in the
low 47. **Crucial contrast:** SM pointers are **not** pointer-cheap —
`toObject()` must mask with `JSVAL_PAYLOAD_MASK` before deref. Otter's `Value`
(`value/mod.rs`) is JSC-style **verbatim** (top16=0, free deref), strictly
cheaper than SM on the hot pointer-load path. Second, bigger divergence: SM
property slots are **full 8-byte `JS::Value`** (`HeapSlot`); SM has **no
compressed slot** — it rejected V8-style compression, trading 2× slot memory for
zero decompress + a uniform slot/Value type. Otter's `CompressedValue` u32
(`compressed.rs:76`) is the V8 point in the space, not SM's. Net: Otter's
*register* Value beats SM; Otter's *slot* Value is a different, sound tradeoff
(a decompress per read, but half the memory and the 4 GiB cage).

### 3.2 Object / property storage
SM `NativeObject`: `Shape* shape_` at offset 0 ≡ Otter `OBJECT_BODY_SHAPE_OFFSET
==0`. Three-way store: **fixed inline slots** (`getFixedSlots()`, in-cell, count
baked into the AllocKind) ≡ Otter `inline_values[4]`; `slots_` (malloc'd dynamic
named props past the fixed count) ≡ Otter overflow `values` Vec; `elements_`
(malloc'd dense indexed, pointing past an `ObjectElements` header with
capacity/length) — Otter has **one** slab and pushes element storage to
`ExoticSlots`. SM cells are fixed-size per AllocKind; growth reallocates the
off-cell `slots_` ≡ Otter's fixed body. Dictionary-mode `Shape` (per-object
`MutablePropMap`) vs shared `SharedPropMap` ≡ Otter `dictionary_shape_id` /
`shape_cache_mode`. SM keeps proto on the BaseShape; Otter on the body
(`jit_proto`).

### 3.3 Write barrier + remembered set + minor GC — **the priority answer**
SM's nursery minor GC **does not scan cards** and **does not walk objects on a
dirty region.** It consumes a **precise StoreBuffer** (`gc/StoreBuffer.h`) whose
entries *are* the only roots into the nursery.

1. **Post-write barrier:** on storing a GC value into a **tenured** cell,
   `HeapSlot::post` checks `IsInsideNursery(child) && !IsInsideNursery(owner)`
   then appends an edge — same predicate as Otter `parent.is_old() &&
   child.is_young()` (`barrier.rs:76,79`).
2. **StoreBuffer = several typed dedup `HashSet`s:** `ValueEdge` (address of a
   `Value` field with a *stable* address, e.g. inside a tenured cell),
   `CellPtrEdge` (single `Cell*` field), and **— the one that solves Otter's
   wall — `SlotsEdge`**: it does **not** store a raw slot address; it stores
   **`{NativeObject* owner, kind (Slot|Element), uint32 start, uint32 count}`**.
   `WholeCellBuffer` (a per-arena cell bitmap) records "re-trace this whole
   tenured cell" for bulk/initializing writes. `GenericBuffer` holds
   trace-closures for exotic edges.
3. **Minor GC** (`Nursery::collect → TenuringTracer`): traces the buffer
   (`traceSlots` resolves `owner->getSlotsHeader()`/`getDenseElements()` **at
   trace time** and traces exactly slots `[start, start+count)`), evacuating
   each nursery referent and overwriting the edge with the new location, then a
   Cheney fixpoint (`collectToObjectFixedPoint`) over freshly-promoted bytes.
   **That is the entire old→young scan.** No card table, no
   `scan_old_dirty_cards`, no header walk, no whole-object re-trace.

**The off-page-slot wall — dodged, not hit.** SM's `slots_`/`elements_` are
malloc'd off-cell **exactly like Otter's overflow Vec** — identical physical
storage. SM avoids the wall because **`SlotsEdge` keys on (owner object, index
range), never on the slot's machine address.** The owner is guaranteed tenured
(barrier predicate) and tenured cells **do not move** during a minor GC, so the
owner pointer is stable even if the mutator realloc'd `slots_`; at trace time SM
re-reads `owner->slots_` to get the current base. **This is precisely the
indirection Otter already performs** in `trace_slots_safe` (refresh `values_ptr`,
then walk). So SM never names a page for an off-heap slot, never fabricates a
header, never over-traces.

**Maps onto Otter's three wastes:** (a) the every-header walk — SM visits only
buffered owners/edges, O(writes); (b) whole-object re-trace — `SlotsEdge` traces
only the written range; `WholeCell` is the *explicit opt-in* for bulk writes,
not a forced card-granularity default; (c) the double pass + re-dirty — SM has
**one** pass (the buffer *is* the root injection), and the re-dirty
(`remember_parent_card_for_young_child`) exists **only** because Otter ages
survivors (`PROMOTE_AFTER_SURVIVALS=1`, page `survival_age`) so a promoted-old
cell can still point at a survived-young cell. **SM tenures all survivors
immediately**, so after every minor GC no old→young edge survives and the buffer
is cleared wholesale. Otter's re-dirty is a consequence of its **aging policy**,
not inherent.

**`P4`'s verdict is "too strong":** intractable for a slot-*address* buffer,
but **tractable for a (owner, slot-index) buffer**, exactly as SM does for
off-`slots_`/`elements_` edges.

### 3.4 Frame / stack ABI
C++ interpreter on `InterpreterFrame`s; JIT uses `BaselineFrame`
(`jit/BaselineFrame.h`) — a stable on-stack ABI shared by Baseline interpreter +
JIT: fixed header (flags, env chain, args object, return value, frameSize) +
value slots; `JitFrameLayout` for the call prologue. Ion deopt ("bailout") uses
`Snapshots` (`jit/Snapshots.cpp`) + `RResumePoint`/`RInstruction` to rebuild
`BaselineFrame`s at an exact bytecode PC; `RematerializedFrame` for the debugger.
**Load-bearing for Otter:** `BaselineFrame` layout + Snapshot frame-state is the
fixed ABI that lands Ion deopt precisely back in Baseline — Otter must design a
windowed-frame layout (stable bytecode-local → register-window-slot map) + per-
safepoint frame-state **now, before JIT re-enable**, not retrofit it.

### 3.5 IC contract
**SM CacheIR is the exact model Otter copied.** `CacheIRWriter` emits a linear op
stream over typed `OperandId`s: `guardToObject`, `GuardShape` (→ a `Shape*` stub
field), `LoadProto`, `LoadFixedSlotResult`/`LoadDynamicSlotResult`,
`StoreFixedSlot`/`StoreDynamicSlot`, `AddAndStoreFixedSlot`/`AddAndStoreDynamicSlot`
(transition). Stub fields (shapes/offsets/protos) in `CacheIRStubInfo`; shapes
uniquely-tenured/immortal so a `Shape*` compare is sound. Near-identical to
`cache_ir::CacheStub`/`CacheOp`: `GuardShapeId`≡`GuardShape`,
`LoadPrototype`≡`LoadProto`, `LoadDataSlotResult`/`HasDataSlot`≡`Load*SlotResult`,
`StoreDataSlot`≡`StoreFixed/DynamicSlot`, `StoreAddTransition`≡`AddAndStore*Slot`;
operand 0=receiver / 1=prototype-after-`LoadPrototype` mirrors SM's `OperandId`
threading; interned+immortal+pinned shapes ≡ SM uniquely-tenured. **Key
difference:** SM **compiles** CacheIR to native via `CacheIRCompiler` (real
Baseline IC codegen); Otter **interprets** the op table. Same property/method
split: GetProp/method rides `GetPropIRGenerator` CacheIR (Otter: method
resolution rides the Load stub); array/dense fast paths use dedicated CacheIR
generators (Otter: separate native-fn-identity dispatch outside CacheIR — a
divergence).

### 3.6 How the JIT consumes the VM ABI
**Warp does not re-derive types** — `WarpCacheIRTranspiler`
(`jit/WarpCacheIRTranspiler.cpp`) reads the **same CacheIR op stream** the
Baseline IC recorded and lowers each op to Ion MIR (`GuardShape`→`MGuardShape`,
`LoadDynamicSlotResult`→`MLoadDynamicSlot`), baking stub-field shapes/offsets as
constants. So the VM guarantees: (1) a stable CacheIR op contract + stub-field
layout (the IC *is* the optimizer's front-end); (2) immortal Shape identity for
baked guards; (3) fixed offsets (`shape_`@0, slot offsets); (4) `BaselineFrame`
+ Snapshot frame-state for exact-PC bailout. **Transferable to Otter:** treat the
`CacheStub` op stream as a *versioned ABI the JIT transpiles* (Warp-style),
not just an interpreter IC; freeze the `CacheOp` stream + tables; keep shape@0 /
`values_ptr` / `jit_proto` bakeable; keep the windowed frame + exact-PC deopt.

---

## 4. Hermes (Meta / React Native) — Hades: evacuating young gen + mostly-non-moving old gen

### 4.1 Value ABI
`HermesValue` is a 64-bit NaN-box (8 bytes ≡ `Value`) but with **opposite
polarity** to Otter: Hermes favors **doubles** (verbatim); every non-double sits
in the quiet-NaN region with a ~16-bit top tag (Empty/Undefined/Null/Bool/Symbol/
Native(U)Int/Str/BigInt/Object), pointer payload in low 48 (or 32 compressed).
Otter is the inverse — **pointers verbatim** (top16=0), doubles pay `+2^49`.
Otter optimizes deref; Hermes optimizes float arithmetic. Two GC-relevant points:
(1) Hermes splits a GC-aware in-heap `GCHermesValue` from a transient
`HermesValue` ≡ Otter's `Value`(register)/`CompressedValue`(slot) split, except
Hermes' in-heap slot is **full 64-bit** unless `HERMESVM_COMPRESSED_POINTERS`,
whereas Otter is **always** a 32-bit `CompressedValue` with a 3-bit tag —
**Otter is more aggressive on slot density.** (2) With compressed pointers
Hermes uses a `CompressedPointer` (u32) decoded **segmented**: `segmentMap_[raw
>> kLogSegmentSize] + (raw & lowMask)`. Otter `RawGc` decodes **flat**:
`cage_base() | offset` — a single OR, no segment-map load (`compressed.rs:70`).
**Otter's flat-cage decode is strictly cheaper per deref**; the cost is a moving
collector (must rewrite the 4-byte slot in place on evac) where Hermes'
segmentation buys a non-moving old gen. **Do not copy the segment-map decode.**

### 4.2 Object / property storage
Near-identical, which *validates* `ObjectBody`. Hermes `JSObject`:
`clazzDoNotAccessDirectly_` (`GCPointer<HiddenClass>`) **first** ≡ Otter
`shape`@`OBJECT_BODY_SHAPE_OFFSET==0`; `parent_` (`GCPointer`, the **prototype**)
**on the object** ≡ Otter `jit_proto`@`OBJECT_BODY_JIT_PROTO_OFFSET`;
`propStorage_` (`GCPointer<PropStorage>`) the overflow array; then
`directProps_[DIRECT_PROPERTY_SLOTS]` inline (`DIRECT_PROPERTY_SLOTS == 5`) ≡
Otter `inline_values[INLINE_SLOT_CAP==4]`. Read: `HiddenClass` maps name→
`SlotIndex`; `slot < DIRECT` reads `directProps_` else
`propStorage_->at(slot-DIRECT)` ≡ Otter shape-guard then
`decompress(values_ptr[slot])`. `HiddenClass` = transition tree, dictionary mode
on deletes ≡ Otter `dictionary_shape_id`/`shape_cache_mode`. **Two structural
differences that matter for GC:** (a) `propStorage_` is a **separate GC-managed
cell** (`PropStorage`, a `VariableSizeRuntimeCell`) **inside a carded segment** —
NOT a malloc Vec; (b) Hermes supports **variable-size cells**
(`VariableSizeRuntimeCell` header carries `size_`), so strings/ArrayStorage/
PropStorage vary and the GC strides them. Otter explicitly does **not** support
variable-size bodies (`GcHeader.size_bytes == sizeof(ObjectBody)`,
`SafeTraceable` size == sizeof), and compensates with `values_ptr` /
`refresh_values_ptr()` so a fixed body can aim at `inline_values` (in-page) or
the Vec (off-page).

### 4.3 Write barrier + remembered set + minor GC — **the priority answer**
This is where Hermes beats `scan_old_dirty_cards`, and the reason is the
off-page wall, **not the card scheme** — Hermes uses cards too, but precisely.

- **Hades young-gen collection** (`HadesGC::youngGenCollection`): roots = the
  register stack (`PinnedHermesValue[]` scanned directly), `GCScope` handle
  chunks, globals, **plus the old→young remembered set**. The remset is a
  **card table per segment** (`CardTableNC.h`): `kLogCardSize == 9` ⇒ **512-byte
  cards** — *identical* to Otter `CARD_SIZE == 512` / `CARDS_PER_PAGE == 512`
  (`page.rs:64,68`).
- **The barrier dirties the card of the SLOT ADDRESS** being written:
  `GCHermesValue::set → HadesGC::writeBarrier → CardTable::dirtyCardForAddress
  (slotAddr)` — **slot-precise carding**, the dirty bit covers the exact 512B
  window containing the mutated field, **not the object header**. Contrast
  `barrier.rs:84`: Otter marks the **parent header's** page card. Otter's cited
  reason is correct (off-page slots in the malloc Vec / exotic Box / shape
  handles have no page to mask to). **Hermes avoids this entirely by having NO
  off-page slots** — `directProps_` are in-cell, `propStorage_` is its own
  carded GC cell, arrays are `ArrayStorage`/`SegmentedArray` cells. Slot-precise
  carding is **only possible because everything is on-page.**
- **No O(objects/page) walk:** Hermes maintains a per-card **object-start /
  crossing map** (`CardTable::boundaries_`, signed-byte exponential-backoff
  encoding). Minor GC uses `findNextDirtyCard` to get maximal dirty-card runs,
  `firstObjForCard(idx)` to land (in O(log)) on the one object crossing into the
  run, then iterates **forward only across the dirty run**. Work = O(objects
  intersecting dirty cards), with the start found in O(log). Otter's
  `scan_old_dirty_cards:498` instead walks **every** header
  `PAGE_HEADER_SIZE..bump_cursor` and re-traces whole intersecting objects.
- **One logical pass:** card roots + stack/handle roots seed the `EvacAcceptor`
  worklist; survivors land in the old gen and drain from the worklist (the
  Cheney-equivalent). No separate dirty-card-retrace → Cheney → re-dirty.
- **Old gen is NON-MOVING** (`doc/Hades.md`: free-list segregated, optional
  *single-segment* compaction per full GC). Non-moving OG is the keystone: old
  objects don't move, so `boundaries_` + cards stay valid and there is **no
  in-place slot rewrite + re-dirty**. Otter's old gen **moves** (promotion +
  future compaction), so `process_slot:217` rewrites slots and must re-dirty —
  which is *why* `P4` called a precise remset intractable. **The real blocker is
  not moving per se; it is the off-page malloc slabs.**

**Recommended ordering Hermes implies:** (1) move overflow `values` Vec + exotic
Box **into the heap as GC cells** (needs `VariableSizeRuntimeCell`-style
variable-size bodies, which Otter lacks) so every slot is carded; **then** (2)
add a per-card object-start/`boundaries_` table to replace the header walk;
**then** (3) dirty the **slot** card, not the parent header. With those three,
Otter gets Hermes-grade precise minor GC **while keeping a moving young gen.**

### 4.4 Frame / stack ABI
Register-based bytecode on a single contiguous `Runtime::registerStack_` of
`PinnedHermesValue`; frames are windows (`StackFrameLayout`/`StackFramePtr`):
local registers then a fixed header (prev frame, saved IP, saved CodeBlock,
argCount, newTarget, thisArg), args below ≡ Otter's P2 flat windowed stack.
**Critical shared GC property:** because the register stack is an array of
`PinnedHermesValue`, the GC marks the live region **directly** as roots —
interpreter values need **no Handle**; only native C++ pays via `GCScope`/
`Handle`. ≡ Otter's `root_slots` over the flat stack. Frame header is
fixed-offset, bytecode bakes register indices — **freeze Otter's windowed-frame
header offsets** so a future JIT bakes them. No spill-to-heap of frames (deep
recursion → StackOverflow), consistent with Otter's flat stack.

### 4.5 IC contract
Far simpler than `CacheStub`: per-site **monomorphic** caches —
`PropertyCacheEntry { WeakRoot<HiddenClass> clazz; SlotIndex slot; }` in the
CodeBlock's read/write-cache arrays, indexed by a cache slot baked into
`GetById`/`PutById`. Compare `obj->clazz_` to the cached `clazz`; hit ⇒ cached
slot; miss ⇒ full lookup, overwrite the **single** entry. **Monomorphic only**
(no operand file, no poly chain, no proto-walk caching, no transition stub),
`WeakRoot` auto-clears on a dead `HiddenClass`. **Otter's `CacheStub` already
exceeds Hermes** — it caches proto loads (`LoadPrototype`) and add-transitions
(`StoreAddTransition`) that Hermes' flat `(clazz, slot)` cannot. Like Otter,
Hermes rides the property cache for method dispatch and keeps array/builtin fast
paths separate ≡ Otter's native-fn-identity dispatch. **Lesson: do not regress
toward Hermes;** the only takeaways are the cheap `WeakRoot` self-clearing entry
and the `(HiddenClass-id, slot)` guard shape a JIT can bake.

### 4.6 How the JIT consumes the VM ABI
Hermes ships **no general optimizing JIT** (AOT HBC + interpreter; Static Hermes
is an AOT native compiler), so it is **not** a codegen template — but it *is* a
template for **which offsets to stabilize**, all of which Otter already has: (1)
`clazz_`@0 as the type guard ≡ shape@0; (2) direct-vs-indirect split with a
compile-time `DIRECT_PROPERTY_SLOTS` boundary ≡ `INLINE_SLOT_CAP` + `values_ptr`
+ `refresh_values_ptr()`; (3) prototype on the object (`parent_`) ≡ `jit_proto`;
(4) `PropertyCacheEntry (clazz-id, slot)` as the bakeable IC ≡ freezing the
`CacheStub` monomorphic own-data fast path; (5) fixed register-frame header
offsets ≡ Otter's windowed frame. Hermes carries **no** deopt/frame-state
metadata (no speculative tier), so on Otter's exact-PC deopt requirement Hermes
offers nothing — **take that from V8 Maglev / JSC, not Hermes.** Net: Hermes
teaches *freeze these offsets*, not *change them*.

---

## 5. QuickJS (Bellard + quickjs-ng) — refcounting + trial-deletion cycle collector

### 5.1 Value ABI
Default 64-bit `JSValue` is **not** NaN-boxed — a fat 16-byte tagged union
`{ JSValueUnion u; int64_t tag; }`. The tag drives **refcounting**:
`JS_VALUE_HAS_REF_COUNT(v)` for object/string/symbol/bigint, each payload begins
with `JSRefCountHeader { int ref_count; }`; a store is `set_value`:
`JS_DupValue(new); JS_FreeValue(old)`. **Otter's `Value` is the stronger design
here** — `u64`, 8 bytes, pointer-cheap. QuickJS pays 2× the register/stack width
*because* it must carry an explicit i64 tag to know whether to decref on every
copy/free — the hidden tax of refcounting. Otter's `CompressedValue` u32 slot
has no QuickJS analog: QuickJS never compresses the in-property value — every
`JSProperty` slot is a full **16-byte** `JSValue`. **Otter stores a property in 4
bytes where QuickJS uses 16** — a 4× density win directly relevant to RSS.

### 5.2 Object / property storage
`JSObject` is fixed, small, and holds **zero inline values**: header
(`JSGCObjectHeader`) + flags byte + `class_id` + `JSShape *shape` + `JSProperty
*prop` (out-of-line malloc value array) + `first_weak_ref` + a class-specific
`union u` (fast arrays use `u.array.u.values`). `JSShape` is the hidden class
(inline hash table at negative offsets, `proto` **on the shape**, trailing
`JSShapeProperty {hash_next:26, flags:6; JSAtom atom}`) — the shape carries
**name(atom)+flags only**; values sit in the parallel `JSObject->prop` (16B
each). Shapes are hash-consed in `JSRuntime->shape_hash`, shared, clone-on-write.
**The decisive contrast with Otter's item4:** QuickJS reserves **zero** in-object
value slots, so a 2-prop object is ~48-56B fixed + a `prop` array sized exactly
`prop_count*16`. Otter's item4 did the **opposite** — embedded `inline_values[4]`,
growing the body 72→96B, and that 24B of always-present slack per object is
**exactly the measured RSS +3..+26% regression** (nbody 16→44MB). QuickJS
demonstrates "out-of-line everywhere, tiny fixed body" wins RSS (no per-object
slack; shape sharing amortizes all metadata). Note Otter is *already* denser per
slot (4B `CompressedValue` vs 16B `JSProperty`), so **the regression is the
in-body inline reservation, not the slot encoding.** QuickJS puts proto on the
shared shape (1 pointer, N objects); Otter pays `jit_proto` per body — another
per-object cost for JIT that QuickJS avoids.

### 5.3 Write barrier + remembered set + minor GC — **the priority answer**
QuickJS has **no minor GC, no card table, no remembered set, no store buffer, no
write barrier** — so it cannot exhibit `scan_old_dirty_cards`. Reclamation: (1)
prompt refcounting frees on `ref_count==0`; (2) a periodic trial-deletion **cycle
collector** over `JSRuntime->gc_obj_list` (an intrusive list of cycle-capable
objects only), three bounded passes (`gc_decref` / `gc_scan` / `gc_free_cycles`)
that reach child slots through each object's own `gc_mark` callback —
**O(live cycle-objects)**, no per-page walk, no card-to-object intersection, no
whole-object re-trace driven by a region, no re-dirty.

**This directly answers the off-page-slot wall.** In QuickJS **every** property
value slot is off-object by construction (`JSObject->prop` is always a separate
malloc — Otter's "off-page malloc Vec" case for *all* objects). QuickJS pays zero
penalty because neither mechanism needs a slot→page mapping: refcount inc/dec
operates on the slot wherever it sits, and the cycle collector reaches it through
the object's enumerator, **never through a card.** Otter's wall is a pure
artifact of *(tracing minor GC + card table on a moving young gen)*; QuickJS has
neither.

**The diagnosis QuickJS sharpens** (it lends no algorithm — no minor GC — but
proves the *principle*): the fix is to **record the edge precisely** rather than
re-derive it from a region. The closest tracing analog is SM's StoreBuffer
(§3.3): record **(parent body handle + slot index)**, not a raw slot address —
which survives a backing-store realloc and is exactly what Otter's
`refresh_values_ptr` indirection already makes safe. So the tractable Otter
change: replace card-bit + header walk with an append-only buffer of `(parent
ObjectBody RawGc, slot_index)` pushed by `write_barrier` on old→young; scavenge
iterates the buffer, `refresh_values_ptr` each parent, `process_slot` exactly
that one slot — turning O(objects-on-dirty-page × size) into **O(old→young
edges)**. `P4`'s "intractable" is **too strong**: intractable for a slot-address
buffer, **tractable** for a `(parent-handle, slot-index)` buffer, as SM does for
off-`elements_` edges.

### 5.4 Frame / stack ABI
`JS_CallInternal` builds each frame via `alloca` on the C stack: `arg_buf +
var_buf +` a value stack of compile-time `stack_size`; `sp` is a `JSValue*`
cursor; opcodes push/pop 16B values. Every stack value is **owned** — on return
or unwind the interpreter `JS_FreeValue`s live slots/args/locals (refcount
discipline leaks into the frame ABI). Closures capture via `JSVarRef` (a boxed
refcounted `JSValue`). **Contrast for Otter's future JIT:** (a) QuickJS frames
are **not** addressable from a single contiguous isolate stack (each is a fresh
`alloca`) — hostile to OSR/deopt frame reconstruction; **Otter's flat windowed
stack is the better substrate** for exact-PC deopt. (b) Per-slot ownership is the
refcount tax; Otter's tracing GC means stack slots are **just roots** scanned by
`external_visit`, no per-slot free — keep that; it is what lets the frame layout
stay a stable JIT-bakeable contract.

### 5.5 IC contract
Bellard QuickJS has **no IC** (`OP_get_field` hashes the atom into the shape).
quickjs-ng **added** one: `JSInlineCache` / `OP_get_field_ic` etc., a per-
`JSFunctionBytecode` IC array; each entry `{ JSShape *shape (identity), uint32_t
prop_offset, atom }`, a short open-addressed list (low-degree poly) + an
`ic_watchpoint` for shape-mutation invalidation. Hit: compare receiver `shape`
by identity, index `JSObject->prop[prop_offset].u.value`. Add/transition stores
and proto hits fall to the slow path. **Far thinner than `CacheStub`** — a flat
`(shape-identity → slot-offset)` table, **no** operand file, **no** multi-op
program, **no** `LoadPrototype`/`StoreAddTransition`. **It validates Otter's
monomorphic own-data fast path** (which already bypasses the operand file — that
*is* QuickJS's whole IC) but has nothing matching Otter's transition/proto
CacheOps — those are Otter's strength. Transferable: QuickJS keys on raw
`JSShape*` identity relying on hash-consed stability ≡ Otter "shapes interned +
immortal + pinned" — guard on a pinned `ShapeId` with the same confidence. Fast
arrays (`fast_array` flag + `u.array`) bypass shape/prop entirely, keyed on
`class_id` ≡ Otter's separate builtin dispatch — **idiomatic, not a smell.**

### 5.6 How the JIT consumes the VM ABI
QuickJS has **no JIT**, but its IC depends on the right stable surface: (1)
`JSObject->shape` at a fixed offset guarded by **identity** against an immortal,
pinned, hash-consed shape ≡ keep `OBJECT_BODY_SHAPE_OFFSET==0` + "shapes
interned/immortal/pinned" rock-stable; (2) a shape→slot mapping yielding a fixed
offset usable without rehash ≡ Otter's "shape-guard ⇒ `decompress(values_ptr
[slot])`". **The hazard QuickJS surfaces by negative example:** its `prop` array
can be realloc'd by `resize_properties`, so the IC **re-reads the prop base each
access and caches only the offset, never the base** — making it immune to
realloc. **Otter's JIT bakes `values_ptr`** (`OBJECT_BODY_VALUES_PTR_OFFSET`) —
faster, but only sound because `refresh_values_ptr` re-caches after every
move/grow and `inline_values` lives in-body when `slab_len<=4`. The VM must
therefore promise the future JIT: **`values_ptr` is ALWAYS current after any
GC/grow (never a torn base)** — treat this as a JIT-re-enable **correctness
gate**, not an optimization.

### 5.7 Reject
QuickJS proves refcounting gives low deterministic RSS and zero barrier/card
machinery, but it **cannot move objects** (no compaction, no pointer
compression) and pays inc/dec on every copy + a cycle collector. Otter is
committed to a moving, 32-bit-compressed, cage-relative heap (`RawGc`,
`CAGE_ALIGN 1<<32`) — **fundamentally incompatible with refcounting.** The RSS
win Otter actually wants is QuickJS's **compact layout** (out-of-line props,
shape-shared metadata, proto-on-shape), **not** its reclamation model.

---

## 6. V8 — Maglev / Turboshaft tier focus (deopt + representation; the JIT-ABI lens)

§1 covered V8's heap. This section isolates what the **optimizing tiers** demand
of the VM, because it is the load-bearing constraint on every GC proposal: *the
remembered-set and object-layout changes must not break exact-PC deopt or the
moving collector's ability to find optimized-frame roots.*

### 6.1 Representation tracking (Maglev) vs Otter's NaN-box
Maglev assigns each SSA value a **representation** (`Int32` / `Float64` /
`Tagged` / `HeapNumber`) and keeps doubles **unboxed in FP registers** across a
loop, re-boxing (allocating a `HeapNumber`) only at representation boundaries or
deopt. This is V8's substitute for Otter's universal NaN-box: **a JIT does not
need a uniform stack encoding** — it needs a stable *in-heap* encoding
(`CompressedValue`, which Otter has) plus per-value representation. Otter's
memory note (repr-selection with loop-carried unboxed residency) is exactly this;
the VM ABI must expose **per-shape field representation monotonically**
(Smi→Double→Tagged, guarded) so the JIT can keep a field unboxed across a loop
and deopt on violation.

### 6.2 Deopt / frame-state — the non-negotiable contract
`deoptimizer/translated-state` (`TranslationArray`/`FrameTranslation`): per
interpreter register/accumulator, record `{location (register/stack/constant),
representation}` at an **exact bytecode offset**. Eager deopts (guard failure)
and lazy deopts (post-call invalidation) each snapshot the full
`InterpreterFrameState` (`EagerDeoptInfo`/`LazyDeoptInfo`). The deoptimizer
rematerializes: re-box an unboxed `Float64` into a `HeapNumber`, re-tag an
`Int32` into a `Smi`. **For Otter the rematerialization is sharper because of
`CompressedValue` tags:** the deopt map must record whether a reconstructed slot
is a verbatim cell offset (`0b000`), boxed `HeapNumber` (`0b010`), immediate
(`0b100`), or function-id (`0b110`), and how to widen a 4-byte slot into an
8-byte `Value`. **This metadata must survive every GC/layout proposal in this
document** — it is the correctness bar.

### 6.3 Safepoint stack maps — what makes a moving collector coexist with a JIT
At every GC-safe point (call sites, allocation sites) V8 publishes a
`SafepointTable` entry recording which spill slots/registers hold **tagged**
pointers, so the moving GC finds and **updates** spilled roots. Otter's collector
**moves** (Cheney young gen + `RawGc` rewrite-in-place), so the re-enabled JIT
**must** emit safepoint stack maps over the flat windowed Value stack — without
them, a minor GC that evacuates a young object cannot fix a JIT-held register
copy. This is independent of, and complementary to, whichever remembered-set
representation Otter chooses.

### 6.4 OSR
`JumpLoop` increments an interrupt/feedback budget; on overflow V8 compiles an
OSR entry for that loop header and transfers the live interpreter frame into the
optimized frame via the same translation machinery (reading the interpreter
register file to seed SSA values). Otter's loop-OSR plan must consume the same
stable interpreter-register indexing the deopt path uses — **one frame-state
contract serves both directions.**

### 6.5 Net VM-ABI mandate (V8 optimizing tier → Otter, JIT frozen)
Before JIT re-enable, **freeze** (do not change): shape@0 / `jit_proto`@
`OBJECT_BODY_JIT_PROTO_OFFSET` / `values_ptr`@`OBJECT_BODY_VALUES_PTR_OFFSET` and
their invariants; define a **copy-on-compile `CacheStub` snapshot**; define an
**exact-PC frame-state record** (interp-reg → {location, `CompressedValue`
representation}); define **safepoint stack maps** over the flat Value stack;
expose **per-shape field representation** monotonically. None of these are JIT
edits — they are VM-side contracts the frozen JIT will later lower against.

---

## 7. BEAM (Erlang/OTP) — immutable terms, age-ordered heap, no write barrier

### 7.1 Value ABI
`Eterm` = one machine UWord, **low-bit** pointer-tagged (the opposite axis from
Otter's high-bit NaN-box): primary tag low 2 bits — `00` HEADER/CP, `01` CONS,
`10` BOXED, `11` IMMED1. A pointer term is a **real full-width aligned pointer**,
`ptr_val(x) = (Eterm*)(x & ~3)` — **no compression, no cage, whole 64-bit space.**
Smalls are immediate (~60-bit). **Floats are BOXED** (header + 8B payload), the
inverse of Otter (doubles immediate, pointers verbatim). BEAM has **no compressed
slot** — every heap slot is a full `Eterm`, so it never pays Otter's `RawGc`
decompression but doubles slot footprint. Shared idea: low-bit/tag discrimination
so the collector knows roots. **BEAM's self-describing HEADER word (arity +
subtag) is the feature Otter lacks at value level** — see §7.2.

### 7.2 Object / property storage
No JS-object / hidden-class concept; structured terms (tuple/cons/map) are heap
blocks whose **first word is a self-describing header** encoding arity+subtag, so
**variable-size bodies are native** (`make_arityval(n)` ⇒ stride by `arity+1`).
The property analog is the **map**: small = `flatmap_t` = `{header, size,
KEYS-pointer (shared sorted key tuple), then size value words inline}`; large =
HAMT. The keys-tuple is the BEAM "shape": `maps:put` on an existing key allocates
a new flatmap **pointing at the same keys tuple** — structural sharing ≡ Otter's
`ShapeHandle`@`OBJECT_BODY_SHAPE_OFFSET==0` (interned/immortal/non-moving keys +
flags) + per-object value slab. Differences vs `ObjectBody` (96B): (1) BEAM's
keys-tuple is an ordinary movable heap term traced like any term; **Otter's shape
is pinned in old space and never traced as a movable slot — a strictly better
moving-GC invariant worth keeping.** (2) BEAM values are inline in a
**variable-length** body; Otter splits `inline_values[4]` vs off-line `values`
Vec by `slab_len<=4`. BEAM proves variable-size-inline is viable *because the
header authoritatively carries the word-count*; Otter cannot today
(`GcHeader.size_bytes` fixed per type, `SafeTraceable` size == sizeof). (3) No
`jit_proto`-on-object analog.

### 7.3 Write barrier + remembered set + minor GC — **the priority answer**
**BEAM has NO generational write barrier and NO remembered set** for the process
heap — nothing like `scan_old_dirty_cards`. It earns this through two structural
facts Otter **cannot** replicate wholesale: (a) terms are **immutable**, (b) the
heap grows monotonically so **age == address order**. If `X` references `Y`, `Y`
existed when `X` was built (you cannot mutate `X` to point at a younger `Y`), so
`Y` is older = at a lower address. After a minor GC tenures everything below
`high_water` into `old_heap`, an old→young edge is **structurally impossible.**

- **Minor GC** (`do_minor`, `erl_gc.c`): (1) rootset = stack words
  `p->stop..hend` + X registers + process dictionary; (2) for each root **move**
  the pointee — below `high_water` → copy to `old_htop` (tenure), else → young
  `n_htop` — writing an `IS_MOVED_*` forwarding tag + new address into the
  from-space first word (≡ Otter `GcHeader::write_forwarding_offset`); (3) **one
  Cheney sweep** with two scan fingers over the new young region AND the
  freshly-tenured old region (`sweep_one_area`) to convergence. It **never walks
  the pre-existing old heap, never reads a card, never re-traces an old object.**
  Watermark tenuring ≡ Otter `PROMOTE_AFTER_SURVIVALS=1` / page `survival_age`.
  Immortal literals skipped by a pointer-range check (`erts_is_literal`) ≡
  Otter's pinned interned shapes.
- **Mapping onto Otter's exact costs:** `scan_old_dirty_cards:498` walks every
  header (O(objects/page)), intersect-tests, and `trace_one`s the **whole**
  object on overlap — no object-start table, no slot granularity, so one mutated
  4-byte slot re-traces a whole multi-field object *and* walks all non-dirty
  neighbors to find it. Plus `process_slot:217` →
  `remember_parent_card_for_young_child:259` re-dirties the parent card, feeding
  the next walk, and `cheney_scan:575` is a separate sweep. **BEAM collapses all
  of this into one Cheney pass because there is no old generation to consult.**
- **The off-page-slot wall — BEAM has it and dodges it the way Otter already
  leans.** BEAM has off-arena storage holding term pointers: `mbuf`
  (`ErlHeapFragment` malloc'd term blocks) and `off_heap`/MSO (refcounted
  `ProcBin`/funs/external-pid list) — **exactly** Otter's case (GC-traceable
  slots in malloc storage outside any page, the reason `barrier.rs` marks the
  parent header's card). **BEAM's dodge is NOT cards and NOT address-masking:**
  it **anchors** each off-arena region as an explicit **linked list on the
  Process struct** (`p->mbuf`, `p->off_heap.first`) and the GC **walks those
  lists as extra roots** — fragments copied wholesale then freed, MSO walked to
  fix refcounts. **Same philosophy as Otter** tracing *through* the parent
  (`trace_slots_safe` walks `slab_len` words from a refreshed `values_ptr`,
  reaching the off-page Vec via the live body) — both refuse to fabricate a page
  from a non-heap slot.

**Two-sided lesson:** (1) Otter's "remember the parent, never the slot" is
**validated** — BEAM does the moral equivalent (anchor on the owning
object/process, enumerate, never mask the wild slot). (2) BEAM is cheaper *only*
because immutability keeps these lists tiny/transient. Otter, being mutable, must
keep a remembered structure — but the fix to `scan_old_dirty_cards` is precision:
replace the whole-page header walk with a **per-card object-start table** (a
dirty card jumps straight to its owning object) **plus**, for off-page-slot
bearers, an explicit **per-old-page side list** of objects whose dirty card means
"re-scan my `values` Vec / `exotic` Box" — i.e. **adopt BEAM's anchor-and-
enumerate for the off-page tail while keeping cards/precision for the in-body
`inline_values[4]` tail.** `P4`'s "intractable" is dodged by anchoring the
remembered entry on the **object** (whose body you already trace), which Otter
**already half-does.**

### 7.4 Frame / stack ABI
Register bytecode VM: X registers (flat global `x[0..1023]`) + Y registers
(frame-local). **Radical contrast:** BEAM's **stack and heap share one
per-process arena** growing toward each other (heap up from `htop`, stack down
from `hend`); collision = GC/grow. Consequence: the rootset is literally the
stack word range `p->stop..hend` scanned as tagged `Eterm`s, with CPs
distinguished by the HEADER tag (`00`) so the collector doesn't follow them as
data. **Otter is the opposite by design** — a flat per-isolate Value stack
**separate** from GC pages; the GC reaches stack values via the handle stack /
`external_visit` (`scavenger.rs`), not by scanning an interleaved arena. BEAM's
shared-arena trick gives near-free rootset scanning + cheap bump-frame alloc but
couples stack growth to GC; **Otter's separation is friendlier to a moving
collector + future JIT** because frame layout is stable and page-independent.
Portable bit: tag in-frame slots / segregate CPs so a precise stack scan is
possible — Otter already gets this (frame slots are Values with the same
discrimination).

### 7.5 IC contract
BEAM has essentially **no IC / shape-guard** to compare — and that absence *is*
the lesson. No hidden classes, no megamorphic property sites: field access is
`element/2` compiled to fixed decision trees; map access is a HAMT/flatmap
lookup. Nearest analogs: the export/atom tables (global hashes, not per-site
caches), the per-module literal pool, and under BeamAsm **direct call sites
patched once the callee loads** (akin to a JSC `CallLinkInfo` / Otter's
compiled→compiled direct-call linking, but for *static* module calls). **So BEAM
offers nothing to import for the property-IC contract** — Otter's
`CacheStub`/`CacheOp` (V8/SM/JSC lineage) is already the right shape with no BEAM
equivalent. One transferable note: BEAM resolves BIFs by **static identity**, not
a shape guard ≡ Otter keeping Array/Collection method ICs **outside** CacheIR
(native-fn-identity dispatch) — a sound boundary, corroborated.

### 7.6 How the JIT consumes the VM ABI
**BeamAsm** (OTP 24+) is a clean "VM provides stable offsets, JIT bakes them"
model — **non-optimizing** (one fixed asm template per BEAM instruction, no
SSA/regalloc/speculation). It bakes as hard ABI: (1) PCB field offsets
(`c_p->htop`, `c_p->stop`, `c_p->hend`, the X register base); (2) the `Eterm` tag
bit layout (pointer-vs-immediate test, inline small unbox); (3) header
arity/subtag (stride, box/unbox tuples/floats). Its alloc fast path is the
template Otter wants for barriers: inline `htop += need; if (htop > hend) call
garbage_collect` — bump + limit compare + **cold** call to a GC stub, never a
per-allocation bridge. **That is exactly Otter's stated discipline** ("emit
guards/barriers inline; stubs only for cold fallback"): the future Otter JIT
should inline the generational barrier (the parent-card mark from `barrier.rs`,
or its successor) and the bump/limit alloc check, cold-calling only on
overflow/old-parent. Translating to the bakeable VM ABI Otter must freeze:
shape@0, `OBJECT_BODY_VALUES_PTR_OFFSET`, `OBJECT_BODY_JIT_PROTO_OFFSET`, stable
`GcHeader` flag bits (`FLAG_YOUNG`@`header.rs:38` + mark color), the barrier
formula, the `CacheStub` layout, and the windowed-frame layout. **BeamAsm's
discipline — freeze PCB offsets / tag bits / header encoding; inline the hot
bump+barrier, cold-call the rest — is the contract template; its instruction
selection is not worth importing.**

---

## 8. Cross-Engine Synthesis

### 8.1 The minor-GC remembered-set question, by engine

| Engine | Mechanism | What is recorded | Minor-GC consumption | Off-page slots? | Why this choice |
|---|---|---|---|---|---|
| **V8** | Per-page **SlotSet** (bucketed bitmap, 1 bit / tagged slot) | **exact slot address** | Iterate set bits → evacuate that one slot; `KEEP/REMOVE_SLOT` prunes inline | **None** — overflow `PropertyArray`/`elements` are HeapObjects on pages | Moving collector + pointer compression; slot-precise is cheapest when every slot is page-addressable |
| **SpiderMonkey** | **StoreBuffer** of typed dedup sets; `SlotsEdge` for ranges | `ValueEdge` = address; **`SlotsEdge` = (owner, kind, start, count)** | Replay buffer as the *complete* root set; trace exactly the recorded slots/ranges | **Yes** (`slots_`/`elements_` malloc) — dodged by keying `SlotsEdge` on **owner+index**, resolved at trace time | Moving nursery + off-cell storage; index edges survive realloc, tenured owner is stable |
| **JavaScriptCore** | **Object-granular** list of mutated parent cells (`WriteBarrierBuffer` → remembered set), CellState dedup | **parent cell pointer** | Visit each remembered parent, `visitButterfly` re-walks its off-line region | **Yes** (Butterfly malloc) — dodged by remembering the **owner cell**, never the butterfly slot | Non-moving sticky-mark gen; coarse re-trace is fine because find-cost is zero |
| **Hermes (Hades)** | **Card table** (512B) + per-card **object-start/boundaries table** | **slot's card** (slot-precise carding) | `findNextDirtyCard` runs + `firstObjForCard` (O(log)) → scan only the dirty run | **None** — `propStorage_`/arrays are GC cells in carded segments | Embeddable, non-moving old gen; cards stay valid without slot rewrites |
| **QuickJS** | **None** (refcount + cycle collector) | n/a (`set_value` = dup/free on the slot in place) | n/a — no minor GC; cycle collector reaches slots via per-object `gc_mark` | **Yes** (every `prop` array is malloc) — irrelevant; no card/page mapping ever needed | Refcount cannot move; deterministic low RSS, no barrier machinery |
| **BEAM** | **None** for the process heap | n/a (immutable; age == address order ⇒ no old→young) | One Cheney sweep over young + freshly-tenured; never consults old heap | **Yes** (`mbuf`/MSO malloc) — dodged by **anchoring** off-arena regions as linked lists on the Process, walked as roots | Immutability + monotonic heap removes the need entirely |
| **Otter (today)** | **Card table** (512B) + **full per-page header walk** | **parent header's card** | `scan_old_dirty_cards`: walk every header, intersect-test, re-trace whole object | **Yes** (`values` Vec / `exotic` Box) — handled by recording the **parent card** + re-tracing whole owner | Moving Cheney young gen + `RawGc` compression; precise set shelved (`P4`) as "intractable" |

### 8.2 The three candidate precise designs vs Otter's constraints

Otter's fixed constraints: **moving** young gen (Cheney) + **32-bit compressed**
pointers/slots (`RawGc`/`CompressedValue`, justified *only* by movement) +
**single heap/cage** + **off-page malloc** overflow (`values` Vec, `exotic` Box)
+ **manual rooting** + **exact-PC deopt metadata** must survive.

1. **Slot-precise store buffer (V8 SlotSet / SM ValueEdge — slot ADDRESS).**
   *Fits only if* every recordable slot is page-addressable. Today it is **not**
   (off-page Vec/Box) → this is the variant `P4` correctly judged intractable.
   Becomes viable **only after** overflow storage moves on-heap (V8/Hermes
   model). Highest precision, highest prerequisite cost.
2. **Index-precise store buffer (SM `SlotsEdge` — (owner, kind, start,
   count)).** *Fits today* — keys on the **tenured/old owner + slot index**, not
   the slot address; resolves `owner.values_ptr` at trace time (Otter's
   `refresh_values_ptr` indirection already makes this safe). Dodges the wall
   without moving storage on-heap. Medium precision (slot-range, not whole
   object), low prerequisite. **The narrowest change that refutes `P4`.**
3. **Object-granular remembered set (JSC parent-cell list / BEAM anchor-and-
   enumerate).** *Fits today* — record the **parent `RawGc`** (in-page, has a
   header) deduped by a `GcHeader.flags` bit (a `FLAG_REMEMBERED` next to
   `FLAG_YOUNG`); re-trace the whole owner via `trace_slots_safe`. Eliminates the
   **find-cost** (no header walk) but keeps coarse whole-object re-trace
   (acceptable because find-cost is now zero — JSC's bet). Lowest prerequisite,
   lowest precision.

**Orthogonal accelerator (Hermes / BEAM):** a **per-card object-start /
boundaries table** kills the O(objects/page) header walk *without* changing the
barrier — a dirty card jumps to its one owning object instead of striding all
headers. This composes with the current card table as an incremental step, and
with designs (2)/(3) for the in-body `inline_values[4]` tail.

**Orthogonal prerequisite (V8 / Hermes / BEAM):** **variable-size bodies** +
**moving overflow storage on-heap** (an Otter `PropertyArray` analog) is what
*fully* dissolves the wall and unlocks design (1). It is the largest change
(touches `GcHeader.size_bytes`, `SafeTraceable`, `refresh_values_ptr`), and BEAM
proves it is viable when the header authoritatively carries the word count.

### 8.3 Convergent findings across all seven engines

1. **`P4`'s "intractable" verdict holds only for the slot-ADDRESS variant.**
   JSC (object-granular) and SM (`SlotsEdge` index-granular) are existence proofs
   that a precise remembered set coexists with **off-page** (Butterfly / `slots_`)
   storage — you key on the in-page **owner**, never the off-page slot. Otter
   **already half-does this** (remember the parent; re-trace via `trace_slots_safe`).
2. **The off-page wall is self-inflicted by the malloc Vec.** V8 and Hermes have
   *zero* off-page slots because overflow storage is a managed GC cell; that is
   the move that makes the slot-precise variant (1) possible. QuickJS and BEAM
   prove that if you keep off-page storage you must instead **anchor on the
   owner** (refcount-in-place / linked-list enumeration).
3. **The double-pass + re-dirty is a tax of MOVING + AGING, not inherent.** JSC
   (sticky-mark, no copy) and SM (tenure-all-survivors-immediately) have no
   re-dirty; Otter's `remember_parent_card_for_young_child` exists because it
   ages survivors (`PROMOTE_AFTER_SURVIVALS=1`). Folding evac-minted edges into
   the same precise mechanism (push owner / re-insert edge) removes the separate
   pass.
4. **The O(objects/page) header walk is pure card-table imprecision.** Every
   precise engine either has no walk (V8/SM/JSC) or replaces it with an
   object-start table (Hermes/BEAM). It is the single most wasteful line in
   `scan_old_dirty_cards`.
5. **Otter's value/object ABI is already engine-grade and should be FROZEN, not
   redesigned.** `Value` ≡ JSC `JSValue` (pointer-cheap, beats SM's masked
   punbox64 and is cheaper than Hermes' segment-map decode); `CompressedValue` ≡
   V8 `Tagged_t` (the right compression; SM/JSC/QuickJS use wider slots); shape@0
   / `values_ptr` / `jit_proto` match every engine's bakeable guard words;
   `CacheStub` is materially SM CacheIR + JSC `AccessCase`, richer than
   Hermes/QuickJS. The action is to **freeze these as the JIT-bakeable contract**.
6. **The JIT-ABI contract is identical across V8/JSC/SM and must be designed now,
   JIT frozen.** All three demand: stable object offsets, immutable/pinned
   hidden-class identity, a snapshot-able IC op stream the optimizer transpiles
   (Warp/Maglev), exact-PC deopt frame-state with {location, representation}
   (sharpened for Otter by `CompressedValue` tag reconstitution), and safepoint
   stack maps so the moving GC updates optimized-frame roots. Hermes/QuickJS/BEAM
   confirm *which offsets to stabilize* but offer nothing for deopt (no
   speculative tier) — take deopt from V8 Maglev / JSC.

> **Pointers to the decision docs:** the *choice* among designs (1)/(2)/(3), the
> object-start-table step, and the variable-size-body / on-heap-overflow
> question are decided in `VM_JIT_FRIENDLY_REFACTOR_PLAN.md` (which owns the `P4`
> remembered-set verdict this document revisits) and `VM_ABI_AUDIT.md` (which
> owns the frozen offset/contract inventory). This document only diagnoses and
> compares.
