# VM_GC_REDESIGN.md — Minor GC, Remembered Set, and Write Barrier Redesign

**Status:** design (no code changes land with this document).
**Scope:** the young-generation scavenger, the generational remembered set, the
write barrier, and the `item4` inline-slot decision. The JIT stays frozen/off;
this document specifies the *VM ABI* the re-enabled JIT will later lower against,
never JIT code.
**Bar:** every proposal must preserve the three load-bearing invariants —
(1) moving-GC correctness (Cheney forwarding + in-place slot rewrite),
(2) manual rooting (`root_slots` + `external_visit`), and
(3) exact-PC deopt frame-state metadata (untouched here; JIT is frozen).
**Gate:** `cargo test -p otter-vm` (644) + `OTTER_GC_STRESS=32/64/128` + per-bench
interpreter timing. Breaking changes are encouraged (single binary, no
back-compat).

---

## 0. TL;DR — the recommendation in one paragraph

Replace the per-page **card bitmap + full-page header walk** with a **per-isolate
object-granular remembered set**: a `Vec<RawGc>` store buffer of mutated *old
parents*, deduplicated by a new `FLAG_REMEMBERED` bit in `GcHeader.flags`. This is
the **JavaScriptCore / QuickJS object-granular model**, and it is the *only* one of
the five surveyed designs that needs **neither** variable-size bodies **nor**
slot-addressable overflow storage — so it dissolves the off-page-slot wall by
construction rather than by a storage rewrite. It keeps the existing
parent-header-granular barrier contract (the 78 `record_write` sites do **not**
change), keeps the moving collector and 32-bit `RawGc`/`CompressedValue`
compression, removes the `O(objects/page)` header walk entirely, removes the
re-dirty feedback loop, and gives the future JIT a cleaner 2-instruction inline
barrier than card-address arithmetic. `item4` (the 4-slot inline body) loses its
*only* stated justification under this design and is **shrunk** (EVOLVE, not
revert), which is also the fix for the nbody RSS anomaly. Slot-precision
(SpiderMonkey `SlotsEdge` / V8 `SlotSet`) and variable-size in-object bodies are
**explicitly deferred**: they are large, JIT-ABI-touching, and buy a second-order
win (avoiding whole-object re-trace) that the counters added in Step 0 must
*prove* is worth it before any work starts.

---

## 1. The double-pass question — is the current minor GC doing redundant work?

### 1.1 Ground truth: the three phases of `scavenge`

`scavenge` (`crates/otter-gc/src/scavenger.rs:129`) runs, after the two root
passes (`scavenger.rs:157` explicit `root_slots`, `scavenger.rs:164`
`external_visit`):

1. **`scan_old_dirty_cards`** (`scavenger.rs:172`, body at `:498`).
2. **`cheney_scan`** (`scavenger.rs:177`, body at `:575`).
3. **`process_slot` → `remember_parent_card_for_young_child`** (`scavenger.rs:217`,
   re-dirty at `:259`), invoked from inside every trace.

### 1.2 Concrete old→young trace (proving the waste is real)

Take an **old** `ObjectBody` `O` (already promoted) that stores a **young**
`ObjectBody` `Y` into one string-keyed slot.

| # | Event | Code | Cost |
|---|-------|------|------|
| 1 | Store fires the barrier. `O.is_old()` ✓, `Y.is_young()` ✓ → `mark_card(O_header_page_offset)`. One card bit set on `O`'s old page. | `barrier.rs:76`–`:84` | O(1) |
| 2a | Next minor GC, `scan_old_dirty_cards`: `O`'s page has a dirty card. Snapshot dirty offsets, clear bits. | `scavenger.rs:531`–`:533` | O(dirty cards) |
| 2b | **Walk EVERY header** `PAGE_HEADER_SIZE..bump_cursor`, striding by `align_up(size, CELL_SIZE)`, intersect-testing each body against the dirty list to *re-discover* which object owns the card. | `scavenger.rs:539`–`:561` | **O(objects/page)** |
| 2c | For `O` (overlap ✓, `!is_swept` ✓) → `trace_one` → `trace_slots_safe` re-traces **all** of `O`'s slots: `shape`, `jit_proto`, every value slot, every exotic edge — to service **one** dirty edge. | `scavenger.rs:558`, `object.rs:1086` | **O(slots of O)** |
| 2d | `process_slot` evacuates `Y` to to-space, rewrites the 4-byte slot in place. | `scavenger.rs:233`–`:234` | O(1) |
| 2e | `remember_parent_card_for_young_child(O, Y_new)`: `Y_new` is in `NewTo` (still young — `PROMOTE_AFTER_SURVIVALS=1` so a *first-survival* child is **copied, not promoted**), `O` is `Old` → **`mark_card(O)` again**. | `scavenger.rs:246`, `:259`–`:277` | O(1), but… |
| 3 | `cheney_scan` scans to-space + freshly-promoted bytes, evacuating `Y`'s children. `O` is **not** re-scanned here (it is old, not in the to-space/promoted range). | `scavenger.rs:585`–`:627` | O(survivors) |

### 1.3 Verdict — the double pass is real, in three distinct wastes

* **W1 — re-derive parents (`O(objects/page)` header walk).** Step 2b strides
  every header on any page that has *any* dirty card and intersection-tests it,
  purely to *rediscover* which objects own the dirty cards. This is information
  the barrier threw away at step 1 (it recorded a 512 B region, not the object).
  JSC, V8, SM, and QuickJS all hold the parent (or the slot) directly and pay
  **zero** find-cost. This is the dominant waste on dense old pages: hundreds of
  header reads + intersection tests to service a handful of edges.

* **W2 — whole-object re-trace.** Step 2c re-traces *all* of `O`'s slots
  (`trace_slots_safe`, `object.rs:1086`–`:1135` walks `shape`, `jit_proto`,
  `slab_len` value words, plus the exotic block) even though exactly one edge was
  dirtied. For an object with K slots this is K slot visits per dirty edge.

* **W3 — re-dirty feedback loop.** Step 2e re-marks `O`'s card *because the
  evacuated child is still young* (it went to `NewTo`, not old). With
  `PROMOTE_AFTER_SURVIVALS=1` (`scavenger.rs:66`) a surviving child is copied to
  to-space on its **first** scavenge and only promotes on the **next** one. So
  the edge `O→Y` stays old→young across that boundary, and `O`'s card is
  re-dirtied → the *next* scavenge repeats **W1+W2 in full** for `O` before `Y`
  finally promotes out. Each surviving old→young edge therefore pays the
  full-page header walk **at least twice**.

`cheney_scan` itself is **not** redundant with `scan_old_dirty_cards` — they cover
disjoint sets (old parents vs. young/promoted survivors). The genuine redundancy
is **W1 + W2, repeated by W3**. The fix must (a) eliminate W1 by recording the
parent directly, (b) collapse W3 into the same O(1) record mechanism, and
(optionally, later) (c) attack W2 with slot ranges.

---

## 2. The remembered-set representation decision (the crux)

### 2.1 The three candidate directions

The prompt frames three options. Mapped onto the surveyed engines:

| Direction | Engine exemplar | Records | Off-page-slot wall? | Needs variable-size bodies? |
|-----------|-----------------|---------|---------------------|------------------------------|
| **(a)** Precise field-logging `SlotSet` / store buffer keyed on **slot address** | V8 `RememberedSet<OLD_TO_NEW>` (`slot-set.h`), SM `StoreBuffer` ValueEdge | exact slot machine address | **HITS the wall** — `ObjectBody.values` (`object.rs:509`) and `exotic: Box` (`object.rs:558`) are malloc; a slot there has no page header to mask to (`barrier.rs:28`–`:36`) | partially — needs overflow on-heap |
| **(b)** Card + per-card **object-start table**, slot iteration | Hermes `CardTable::boundaries_`, BEAM crossing map | dirty 512 B card + a crossing table to find the owning object | still parent-header-granular today; kills only W1's *walk*, not the find | no |
| **(c)** Store buffer keyed on **parent header + slab index** | SM `SlotsEdge{owner,kind,start,count}`, QuickJS-as-analog | `(owner RawGc, slot range)`; resolve `values_ptr` at trace time | **dodges** the wall (owner is in-page) for the *string-keyed slab only*; exotic edges still need a fallback | no |
| **(c′)** Store buffer keyed on **parent header only** (object-granular) | **JSC** `WriteBarrierBuffer`+CellState, **QuickJS** mark-list | `owner RawGc` (one entry per mutated old parent) | **dodges the wall completely** — never names a slot, re-traces the whole owner via `trace_slots_safe` which already walks inline + off-page Vec + exotic | **no** |

### 2.2 Recommendation: **(c′) object-granular remembered set** as the primary direction

**Take the JSC/QuickJS object-granular design.** Concretely:

* Add a **per-isolate** `remembered_parents: Vec<RawGc>` store buffer to `GcHeap`
  (lives next to `last_scavenge` in `heap.rs:105`; per-isolate, **never**
  `thread_local`).
* Add `FLAG_REMEMBERED = 0b0100_0000` to `GcHeader.flags` (`header.rs:37`–`:59`
  currently uses `0x01`–`0x20`; bits `0x40` and `0x80` are free) with
  `is_remembered` / `set_remembered` / `clear_remembered`, mirroring the existing
  `is_swept`/`set_swept` accessors (`header.rs:222`–`:230`).
* The barrier fast path becomes the JSC `cellState == PossiblyBlack` test:
  `if parent.is_old() && child.is_young() && !parent.is_remembered() { parent.set_remembered(); buffer.push(parent_offset) }`.
* Minor GC iterates `remembered_parents` directly as additional roots, `trace_one`
  on each, and drains/clears `FLAG_REMEMBERED` at the end.

### 2.3 Justification against Otter's four constraints

1. **32-bit compressed `RawGc` slots + moving collector.** Object-granular keys
   on the *parent header offset* (`RawGc`, `compressed.rs`), which is in-page and
   forwarding-stable for an **old** parent (old objects do not move on a minor
   GC — `process_slot` early-returns on `!is_young()`, `scavenger.rs:226`). We
   never record a *slot* address, so we never depend on the slab base staying
   put. Compression is preserved untouched.

2. **Single heap.** No new space, no second remembered structure. The buffer is a
   plain `Vec<RawGc>` in the heap. Full GC clears it (everything re-marked).

3. **The off-page-slot wall.** This is the decisive point. The barrier *already*
   records the parent, not the slot, **precisely because** `ObjectBody.values`
   (`object.rs:509`) and `exotic: Option<Box<ExoticSlots>>` (`object.rs:558`,
   ~140 B) are malloc side-storage with no in-cage slot address
   (`barrier.rs:28`–`:36`). Otter and JSC **made the same correctness decision**;
   the only difference is representation — Otter stores a *card bit* and rebuilds
   the parent set by walking headers (W1), JSC stores the *parent pointer* and
   pays zero find-cost. Object-granular keeps Otter's existing correctness
   decision and simply records the parent it already commits to re-tracing.
   `trace_slots_safe` (`object.rs:1086`) already walks the inline slab, the
   off-page `values` Vec (via the refreshed `values_ptr`, `object.rs:1111`–`:1115`),
   and every exotic edge (`object.rs:1138`–`:1175`) — so re-tracing the whole
   owner reaches the off-page slots with **no slot addresses required**. The wall
   is dissolved, not climbed.

4. **VM_JIT_FRIENDLY_REFACTOR_PLAN P4's "intractable" verdict is too strong.** P4
   judged a *precise* remembered set intractable because of the off-page wall.
   That verdict is correct **only for direction (a)** — the slot-address
   `SlotSet`/`StoreBuffer`. It is **false for (c′)**: JSC is the existence proof
   that a precise *object-granular* remembered set coexists with off-page
   (butterfly) storage, because you remember the in-page **owner**, never the
   off-page slot.

### 2.4 Why not (a), (b), or (c)

* **Reject (a) [slot-address `SlotSet`] as the primary.** It is exactly the
  variant that hits the wall: `ObjectBody.values` and the exotic `Box` give
  malloc slot addresses with no `MemoryChunk`. To make (a) sound you must *first*
  move all overflow + exotic storage on-heap (variable-size bodies), which is the
  large JIT-ABI-touching project §4/§6 argues to defer. (a) is the **eventual**
  ceiling, not the **next** step.

* **Reject (b) [object-start table] as the primary.** A per-card crossing table
  (Hermes `boundaries_`, BEAM) kills W1's *header walk* but Otter's cards are
  still parent-header-granular, so (b) buys nothing over (c′) for the *find*
  problem while adding a second per-page side table to maintain. (c′)'s
  `Vec<RawGc>` **is** the object-start information — already in object form — at
  lower complexity. (b) only becomes interesting if Otter keeps cards for some
  other reason, which this design removes.

* **Defer (c) [`SlotsEdge` slab-index buffer] to a measured Step 3.** It dodges
  the wall for the *string-keyed value slab* by re-reading `values_ptr` at trace
  time (SM's trick, safe here because `refresh_values_ptr`, `object.rs:886`,
  already rebases after any move). But it does **not** cover the exotic edges
  (`symbol_props`, accessor getter/setter pairs, `MappedArgumentsData` cells —
  `object.rs:1146`–`:1174`), which are scattered through the `Box` and are not
  addressable by `(owner, slab_index)`. So (c) needs the object-granular
  whole-cell entry **anyway** as the exotic fallback. (c) is therefore a *partial
  refinement on top of (c′)* — worth it only if Step 0's counters show W2
  (whole-object re-trace) still dominates after (c′) lands. It is Step 3, not the
  foundation.

### 2.5 The single-pass shape after (c′)

```
minor GC:
  1. root_slots                              (unchanged, scavenger.rs:157)
  2. external_visit                          (unchanged, scavenger.rs:164)
  3. for parent in remembered_parents:       (REPLACES scan_old_dirty_cards W1+W2-find)
        if !parent.is_swept(): trace_one(parent)   // whole-owner re-trace (W2 kept)
  4. cheney_scan to fixpoint                  (unchanged, scavenger.rs:575)
  5. ephemeron fixpoint / weak registry      (unchanged, scavenger.rs:183/:189)
  6. clear FLAG_REMEMBERED + drain buffer; re-dirty pushes (W3 → O(1) push)
  7. flip                                     (unchanged, scavenger.rs:207)
```

W1 is gone (parents in hand). W3 collapses: `remember_parent_card_for_young_child`
(`scavenger.rs:259`) becomes `remember_parent` — an O(1) dedup'd
`buffer.push(parent)` instead of a card mark, the **same single mechanism** as the
barrier-time edge (JSC `KEEP_SLOT` / promotion re-insert). W2 (whole-owner
re-trace) is retained, as JSC retains it, because the find-cost is now zero.

---

## 3. The 78-site write-barrier problem — do the sites change?

### 3.1 No. The call-boundary contract is preserved exactly.

The 78 `record_write` call sites in `crates/otter-vm/src` do **not** pass slot
addresses today; they pass `(parent, value)`:

```rust
// heap.rs:1796
pub fn record_write<T: ?Sized, V: GcStore + ?Sized>(&mut self, parent: Gc<T>, value: &V) {
    ...
    value.visit_gc_edges(&mut |edge| self.write_barrier_raw(parent, edge.raw()));
}
```

The doc comment already states the design intent: *"Card marking is
header-granular, so the slot's actual location (inline or malloc-owned side
storage) is irrelevant."* (`heap.rs:1793`–`:1795`). Object-granular keeps this
**verbatim** — it is still parent-header-granular, still takes `(parent, value)`,
still derives nothing from the slot. **The migration is internal to
`barrier.rs` + `scavenger.rs` + `heap.rs`; it does not touch any of the 78 VM call
sites.** This is the single biggest reason to prefer (c′) over (a)/(c): a
slot-precise design would force all 78 sites to surface a slot address or a slab
index, which they structurally cannot do for off-page/exotic edges.

### 3.2 The barrier's new contract

```
write_barrier(parent_header, child, marking):
  // (1) Generational — object-granular remembered set.
  if !child.is_null()
     && (*parent_header).is_old()
     && (*child_header).is_young()
     && !(*parent_header).is_remembered():
        (*parent_header).set_remembered();
        remembered_parents.push(RawGc(parent_offset));
  // (2) Insertion (Dijkstra) barrier — unchanged, dormant under STW.
  ...
```

Contract clauses:

1. **Granularity:** parent-object. One buffer entry per mutated old parent,
   regardless of how many slots in it (inline, off-page Vec, or exotic Box) point
   young. Idempotent via `FLAG_REMEMBERED` (JSC `cellState` dedup) — no duplicate
   entries, so the buffer is bounded by the number of *distinct* dirty old
   parents, not by write count.
2. **Soundness:** the parent is in-page and (being old) does not move on minor GC,
   so its `RawGc` offset is stable across the scavenge that consumes it.
3. **Completeness:** minor GC re-traces every buffered parent in full via
   `trace_slots_safe`, reaching every off-page/exotic slot — so no edge is lost,
   exactly as the current card scan "re-traces every object intersecting a dirty
   card in full" (`barrier.rs:33`–`:36`).
4. **Lifecycle:** entries are consumed each minor GC and cleared (`FLAG_REMEMBERED`
   reset). Re-dirty (an edge still old→young after evacuation) re-pushes the
   parent via the same path. **Sweep interaction:** when a full GC sweeps a dead
   old object (`set_swept`, `header.rs:228`), the drain must skip it
   (`!is_swept()` guard, retained from `scavenger.rs:557`) **and** its
   `FLAG_REMEMBERED` is meaningless after free — full GC clears the whole buffer
   and all remembered bits, rebuilding via the full trace's old→young discovery.
   This is strictly *safer* than today: a precise `Vec` lets us implement V8/JSC
   `RemoveRange`-on-sweep instead of relying on corpse-walking + `is_swept`
   guards.
5. **JIT-bakeable form (future):** the inline barrier the frozen JIT will later
   lower becomes *load `parent.flags` byte at `HEADER_FLAGS_BYTE_OFFSET`
   (`header.rs:43`); test `FLAG_YOUNG` on child; if `old ∧ young ∧ ¬remembered` →
   cold-call push*. Freeze `FLAG_REMEMBERED` as a baked constant beside
   `GENERATION_YOUNG_FLAG` (`header.rs:48`). This is a stable 2-instruction inline
   — **strictly simpler than the current card path** (which needs page-base
   masking `addr & !(PAGE_SIZE-1)` + `card/8` + `1<<(card%8)`, `page.rs:182`–`:186`).

---

## 4. The `item4` (`inline_values[4]`) decision — KEEP / REVERT / EVOLVE

### 4.1 Ground truth

`ObjectBody` is **96 bytes**, pinned by `const _: () =
assert!(size_of::<ObjectBody>() == 96)` (`object.rs:660`). `inline_values:
[CompressedValue; INLINE_SLOT_CAP]` with `INLINE_SLOT_CAP = 4` (`object.rs:566`,
`:574`) plus `slab_len: u16` (`object.rs:568`) is the in-body slab. The session
record: body **72→96 B**, RSS **+3..+26 %**, **+6 % richards**, and **the precise
`SlotSet` it was meant to enable was shelved** — so the cost was paid with no
realized payoff.

`item4`'s **only** stated justification is in its own doc comment: *"a small
object … its slots live in the GC page (the precondition for a precise old→young
remembered set)"* (`object.rs:559`–`:561`).

### 4.2 Verdict: **EVOLVE** (shrink), not KEEP, not REVERT

The decision is entirely determined by §2. Under the object-granular remembered
set, **slot addresses are never recorded** — the remembered entry is the parent
object, and `trace_slots_safe` reaches off-page slots through the refreshed
`values_ptr` anyway. Therefore **`item4`'s sole GC justification evaporates**: it
does not matter for the remembered set whether a small object's slots are in-page
or in a malloc Vec.

But do **not** REVERT to zero inline slots (the QuickJS "out-of-line everywhere"
model). Inline slots have an *allocation-locality* merit independent of GC
precision: they spare a small object a separate `Vec` malloc and keep its hot
slots in the same cache line as the shape/`values_ptr`. **Every surveyed engine
except QuickJS keeps inline slots** — V8 in-object properties, JSC
`inlineCapacity` (default 6), Hermes `DIRECT_PROPERTY_SLOTS` (5), SM fixed slots.
The defect is not "inline slots exist"; it is "`INLINE_SLOT_CAP=4` reserves 4
slots of always-present slack on objects that use fewer," paid by **every** body
including the dominant `{}`/class-instance case.

**EVOLVE = shrink the cap.** Recommended: `INLINE_SLOT_CAP = 2`. Two inline
`CompressedValue`s (8 B) cover the large majority of objects with ≤2 own
string-keyed properties while cutting the body back toward ~80 B and reclaiming
most of the +3..+26 % RSS slack. The spill path already migrates wholesale on
overflow (`object.rs:706`–`:717`: at `len == INLINE_SLOT_CAP`,
`values.extend_from_slice(&inline_values)`), so changing the constant is
mechanically contained — `refresh_values_ptr` (`object.rs:886`) and
`trace_slots_safe` are written against `slab_len`/`INLINE_SLOT_CAP`, not a literal
4. Measure RSS (nbody) and richards via the Step-0 counters and pick 2 vs. 0 vs.
keeping-but-right-sizing empirically.

### 4.3 Should this go all the way to per-shape variable-size in-object bodies?

The V8/JSC/Hermes/SM endgame is *size the inline region from the hidden class*
(`Map::GetInObjectProperties`, `Structure::inlineCapacity`, `DIRECT_PROPERTY_SLOTS`,
`nfixed`) so most objects never touch overflow at all. The prompt asks whether
this is worth it for Otter. **Confronting it head-on:**

Otter bodies are **fixed-size per type**: the `Pelt`/`SafeTraceable` size is
`sizeof(ObjectBody)`, and `GcHeader.size_bytes` is a `u32` set once at alloc
(`header.rs:79`, `:127`). To support a runtime-length in-object slab you would need:

| Concern | What variable-size requires | Difficulty |
|---------|------------------------------|------------|
| **Allocation** | Compute body size from the shape's in-object count at alloc; lay out `header + fixed prefix + [CompressedValue; N]` tail manually. | High — abandons the safe `#[repr(C)] struct ObjectBody` + derive; needs a manual DST / thin-pointer layout (Rust has no native flexible array member). |
| **`size_bytes`** | Already a `u32` and already **variable-capable** — the GC strides by `(*header).size_bytes()` (`scavenger.rs:649`, `:449`), not by `sizeof`. **This part is free.** | Low |
| **Trace stride** | `trace_slots_safe` would read N from the shape (or `slab_len`) and walk the in-body tail; `scan_range_raw` already strides by `size_bytes`. | Medium — re-hand-write `trace_slots_safe` for a tail of runtime length. |
| **JIT baked offsets** | `values_ptr` (`OBJECT_BODY_VALUES_PTR_OFFSET`, `object.rs:642`) would point into the in-body tail — fine — but the body struct can no longer *express* the layout in safe Rust, so every offset (`shape@0`, `values_ptr`, `jit_proto`) must be re-pinned by manual `offset_of` against the hand-laid prefix. | Medium-High — the frozen JIT bakes these; re-pinning is load-bearing for re-enable. |

**Verdict: not worth it now.** The object-granular remembered set captures
essentially the entire minor-GC win **without** variable-size bodies (§2.3). The
`size_bytes`-stride machinery is already variable-capable, but the *Rust type
layout* and the *JIT offset re-pin* are a large, invasive change whose payoff
(avoiding the off-page Vec for medium objects, plus W2 slot-precision) is
second-order. Defer it to a dedicated **"in-object-everywhere"** project paired
with JIT re-enable, undertaken **only if** Step 0's counters show the off-page Vec
and W2 still dominate after Steps 1–2. EVOLVE `item4` to a small fixed cap now;
keep variable-size as a documented future option, not a prerequisite.

---

## 5. The nbody RSS anomaly (16→44 MB)

### 5.1 Hypothesis

nbody allocates a large population of small, medium-lived bodies (point/vector
objects). The body growing **72→96 B** is a 1.33× per-object size increase, but
RSS went **16→44 MB ≈ 2.75×** — **super-linear**. The amplifiers:

1. **Copying semispace doubles young-gen bytes.** The young gen is a Cheney
   from/to pair (`NewFrom`/`NewTo`, `page.rs:94`–`:95`); live young bytes cost ~2×
   in reserved page bytes. A +24 B/object slack is therefore amplified ~2× in the
   young footprint.

2. **`PROMOTE_AFTER_SURVIVALS=1` promotes first-survivors into non-compacting old
   space.** Medium-lived nbody objects survive one scavenge and promote
   (`scavenger.rs:66`, `:452`–`:454`). Old space is reaped only at **whole-page
   granularity at full GC** (per `FLAG_SWEPT` comment, `header.rs:51`–`:59`); it
   does not compact. So the +24 B/object rides into old pages and stays resident
   until a full GC frees an *entire* page. Larger bodies → fewer objects per
   256 KiB page → more pages pinned for the same live-object count → RSS
   multiplies.

3. **Young-gen growth-ratio sizing reacts to bytes, not objects.** If the young
   gen grows its reserve by a ratio on *allocated bytes per cycle*, larger bodies
   inflate bytes/cycle → the young reserve grows faster → more reserved pages,
   compounding (1).

The combination — 2× semispace × larger-body promotion into non-compacting old ×
byte-driven young growth — converts a 1.33× body increase into a ~2.75× RSS
increase. The +24 B is not paid once; it is paid in the young semispace, again on
promotion, and held in non-compacting old pages.

### 5.2 How the redesign fixes it

* **Primary fix — EVOLVE `item4` (§4.2).** Shrinking the cap removes the
  per-object slack at its source, directly deflating all three amplifiers. This is
  the lever the nbody anomaly points at.
* **Measurement, not guesswork.** `ScavengeStats.promoted_bytes` **already
  exists** (`scavenger.rs:74`) and is surfaced as `heap.stats.last_scavenge`
  (`heap.rs:1284`). Step 0 wires it into a per-bench readout so the
  promotion-amplifier hypothesis is **confirmed or refuted with numbers** before
  the body shrinks, and the RSS delta is measured after. If `promoted_bytes` for
  nbody is large relative to `copied_bytes`, amplifier (2) is the culprit and the
  body shrink is the fix; if not, revisit young-gen growth sizing.
* **Secondary, not for RSS.** The object-granular remembered set does not change
  RSS directly (it changes *pause time*), but by removing `item4`'s GC
  justification it *unblocks* the shrink that does fix RSS.

---

## 6. Concrete migration path (ordered, each independently landable + gateable)

Each step is one atomic semantic change (no dual-path feature flags — a banned
pattern; cut over and revert via git if a step regresses).

### Step 0 — Telemetry FIRST (no behavior change)

Add counters so every later step's win is **measured before/after**.

* Extend `ScavengeStats` (`scavenger.rs:69`–`:77`, currently `copied_bytes`,
  `promoted_bytes`, `slot_updates`) with:
  `minor_pause_ns`, `dirty_cards_scanned`, `old_headers_walked`,
  `objects_retraced`, `slots_scanned`, `remset_entries`.
* Populate them in `scan_old_dirty_cards` (`scavenger.rs:498`) and `process_slot`
  (`scavenger.rs:217`): bump `old_headers_walked` per stride in the
  `:539`–`:561` loop, `objects_retraced` per `trace_one`, `slots_scanned` in
  `trace_slots_safe`, `dirty_cards_scanned` in `for_each_dirty_card`.
* Time the pause in `collect_minor_internal` (`heap.rs:1244`) with `Instant`
  (already imported, `heap.rs:37`) around the `scavenge` call (`heap.rs:1271`);
  store into `last_scavenge` (`heap.rs:1284`).
* **Gate:** 644 tests + `GC_STRESS=32/64/128`, identical behavior; baseline
  numbers recorded for nbody/richards.

### Step 1 — Object-granular remembered set (the whole-class win)

Atomic cutover from card bits to the parent store buffer.

1. `header.rs`: add `FLAG_REMEMBERED = 0x40` + `is_remembered`/`set_remembered`/
   `clear_remembered` (mirror `is_swept`/`set_swept`, `header.rs:222`–`:230`).
2. `heap.rs`: add `remembered_parents: Vec<RawGc>` to `GcHeap` (per-isolate).
3. `barrier.rs:62`–`:86`: replace the `mark_card` block with the dedup'd
   `set_remembered` + `push` (§3.2). Keep clause (2) insertion barrier unchanged.
4. `scavenger.rs`: replace `scan_old_dirty_cards` (`:498`) with a loop over
   `remembered_parents` → `trace_one` (keep the `!is_swept` guard, `:557`);
   replace `remember_parent_card_for_young_child` (`:259`) with `remember_parent`
   (dedup'd push); at scavenge end, clear `FLAG_REMEMBERED` on all buffered
   parents and drain the buffer.
5. Full GC: clear the buffer + remembered bits; rediscover via the full trace.
6. Remove the card-table from the minor path. `card_bitmap`/`mark_card` may stay
   in `page.rs` only if some other consumer needs it; otherwise delete to avoid
   dead ABI.
* **Expect:** `old_headers_walked → 0`, `remset_entries` ≈ distinct dirty old
  parents, minor pause down on dense-old workloads (richards). `objects_retraced`
  roughly unchanged (W2 retained).
* **Gate:** 644 + `GC_STRESS=32/64/128` (mixed old→young edges, promotion, weak
  tables) + richards/nbody timing vs. Step-0 baseline.

### Step 2 — EVOLVE `item4` (RSS fix)

* Shrink `INLINE_SLOT_CAP` (`object.rs:574`) 4→2; re-pin
  `assert!(size_of::<ObjectBody>() == …)` (`object.rs:660`) to the new size.
* Confirm `refresh_values_ptr` (`object.rs:886`) and the spill path
  (`object.rs:706`–`:717`) are constant-driven (they are).
* **Expect:** nbody RSS down toward the pre-`item4` 16 MB band; richards −6 %
  regression recovered; `promoted_bytes` (Step 0) confirms the amplifier story.
* **Gate:** 644 + `GC_STRESS` + RSS/timing deltas.

### Step 3 — (Optional, measured) slot-range precision for the value slab

Only if Step-0 counters show `objects_retraced`/`slots_scanned` still dominate
after Steps 1–2.

* Add an SM-style `SlotsEdge { owner: RawGc, start: u16, count: u16 }` store-buffer
  variant for the **string-keyed slab** of large objects; trace by re-reading the
  refreshed `values_ptr` (`object.rs:1111`–`:1115`). Keep the object-granular
  whole-cell entry as the default and the **exotic-edge fallback**
  (`object.rs:1146`–`:1174`).
* **Gate:** 644 + `GC_STRESS` + `slots_scanned` delta proving W2 actually shrank.

### Step 4 — (Deferred, not recommended now) variable-size in-object bodies

Documented in §4.3 as a JIT-ABI-touching project for JIT re-enable. Not on the
critical path; revisit only if the off-page Vec proves to still dominate.

---

## 7. Verification plan

### 7.1 Counters to add **first** (Step 0), and where

| Counter | Where it lives | Where it is bumped |
|---------|----------------|--------------------|
| `minor_pause_ns` | `ScavengeStats` (`scavenger.rs:69`) → `heap.stats.last_scavenge` (`heap.rs:105`, `:1284`) | `Instant` around `scavenge` in `collect_minor_internal` (`heap.rs:1271`) |
| `dirty_cards_scanned` | `ScavengeStats` | `for_each_dirty_card` callback (`scavenger.rs:532`, `page.rs:204`) — baseline only; → `remset_entries` after Step 1 |
| `old_headers_walked` | `ScavengeStats` | header-stride loop (`scavenger.rs:539`–`:561`) — should fall to 0 after Step 1 |
| `objects_retraced` | `ScavengeStats` | per `trace_one` (`scavenger.rs:558`, `:664`) |
| `slots_scanned` | `ScavengeStats` | per slot in `trace_slots_safe` (`object.rs:1116`) |
| `promoted_bytes` | **already exists** (`scavenger.rs:74`, set `:479`) | nbody RSS hypothesis (§5) — read as-is |
| `remset_entries` | `ScavengeStats` | size of `remembered_parents` consumed (Step 1+) |

These let every step report a **before/after delta** (the engine "thermometer"),
not a vibe. The win is measured, not asserted.

### 7.2 Correctness gates (run on every step)

1. **`cargo test -p otter-vm`** — all **644** pass.
2. **`OTTER_GC_STRESS=32` / `64` / `128`** — scavenge-on-every-Nth-alloc; must be
   clean. Exercises: old→young edges (object-granular completeness), promotion
   (`PROMOTE_AFTER_SURVIVALS`, re-dirty → re-push), weak/ephemeron tables
   (`scavenger.rs:183`/`:189`), swept-corpse safety (`!is_swept`, `:557`). Note:
   `OTTER_GC_STRESS≤16` has a **pre-existing** module-root corruption bug
   (documented separately) — not introduced here; gate at 32/64/128.
3. **Invariant audit per step:**
   * *Moving-GC:* forwarding (`write_forwarding_offset`, `header.rs:270`) and
     in-place slot rewrite (`process_slot`, `scavenger.rs:234`) unchanged;
     remembered parents are **old** (non-moving on minor GC), so their `RawGc`
     stays valid across the consume.
   * *Manual rooting:* `root_slots` + `external_visit` (`scavenger.rs:157`/`:164`,
     `heap.rs:1251`–`:1264`) untouched.
   * *Exact-PC deopt metadata:* untouched — JIT is frozen. The only JIT-facing
     change is the **future** barrier ABI (§3.2 clause 5), which freezes
     `FLAG_REMEMBERED` as a baked constant; the stale JIT is not re-enabled here.
4. **Per-bench interpreter timing** (richards for pause, nbody for RSS) compared
   to the Step-0 baseline using §7.1 counters.

### 7.3 What "done" looks like

* Step 1: `old_headers_walked = 0` on every minor GC; minor pause on richards
  down; 644 + stress green.
* Step 2: nbody RSS back in the ~16 MB band; richards +6 % recovered; 644 +
  stress green.
* The off-page-slot wall is **retired as a blocker** — recorded against the
  parent, re-traced whole via `trace_slots_safe`, never masked to a page.

---

## 8. Summary of positions taken

| # | Question | Position |
|---|----------|----------|
| 1 | Double pass redundant? | **Real.** W1 (header walk to re-derive parents) + W2 (whole-object re-trace) + W3 (re-dirty feedback) — traced concretely through `scavenger.rs:498/217/259`. |
| 2 | SlotSet vs. card+object-start vs. in-object-everywhere vs. store buffer? | **Object-granular `Vec<RawGc>` parent store buffer (JSC/QuickJS), deduped by `FLAG_REMEMBERED`.** Dodges the off-page wall by construction; refutes P4's "intractable" (true only for slot-address SlotSet). |
| 3 | 78 (≈95) barrier sites change? | **No.** Parent-header-granular contract preserved verbatim; migration is internal to `barrier.rs`/`scavenger.rs`/`heap.rs`. New contract in §3.2. |
| 4 | `item4` KEEP/REVERT/EVOLVE? | **EVOLVE** — shrink `INLINE_SLOT_CAP` 4→2; its GC justification evaporates under object-granular, but inline-locality merit (validated by 4/5 engines) argues against full revert. Variable-size bodies: **defer** (large, JIT-ABI-touching, second-order payoff). |
| 5 | nbody RSS 16→44 MB? | Super-linear amplification: 2× semispace × first-survivor promotion into non-compacting old × byte-driven young growth, on the +24 B body. Fixed by the Step-2 shrink; confirmed via the already-existing `promoted_bytes` counter. |

---

## 9. Frozen JIT ABI (what this redesign promises the future optimizing tier)

Unchanged and must stay byte-stable: `OBJECT_BODY_SHAPE_OFFSET == 0`
(`object.rs:637`/`:651`), `OBJECT_BODY_VALUES_PTR_OFFSET` (`object.rs:642`),
`OBJECT_BODY_JIT_PROTO_OFFSET` (`object.rs:648`); `CompressedValue` tag constants
(`compressed.rs:34`–`:54`); `Value` NaN-box; the `CacheStub`/`CacheOp` contract.
**Changed for the better:** the inline generational write barrier the JIT will
lower goes from card-address arithmetic (page mask + `card/8` + bit, `page.rs:182`)
to *load `flags` byte at `HEADER_FLAGS_BYTE_OFFSET=1` (`header.rs:43`); test
`FLAG_YOUNG` (`GENERATION_YOUNG_FLAG`, `header.rs:48`) on child; if
`old ∧ young ∧ ¬FLAG_REMEMBERED` cold-call push* — a stable 2-instruction inline.
Freeze `FLAG_REMEMBERED = 0x40` as a baked constant. No deopt/frame-state or
register-window layout changes; those remain exactly as the frozen JIT expects.
