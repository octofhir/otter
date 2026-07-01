# VM Refactor Plan — Forward roadmap (baseline JIT on)

**Scope.** Improve the **VM itself**. Baseline JIT is **on**; the Value + slot ABI
port is complete and `diff.mjs` is 24/24 across tiers. **No back-compat constraint** —
bytecode, object layout, frame model, macros all break freely. Whole-subsystem
rip-and-replace, not safe incremental slices. The only invariants kept are
*behavioral* (diff + test262) and *GC soundness* (no use-after-move). Everything
structural is on the table.

Companion: `VM_ABI_AUDIT.md`.

## Status (2026-07-01, profile-grounded)

Landed:
- **P0 — Value reencoding (JSC pointer-cheap).** LANDED. Pointers stored verbatim
  (top16 = 0, free deref), doubles `+2^49` offset, int32 tagged. The bedrock every
  later slab/guard/IC builds on.
- **P3 — thin `VmError`.** LANDED. `Copy`, ≤24 B, dynamic payload boxed in a
  per-isolate slot; the success path never builds or drops error data.
- **P1 — god-struct split + shape-handle IC hit path.** PARTIAL. The god-struct
  split (hot core `{shape, proto, flags, values_ptr}`, exotic fields boxed) and the
  interned-shape-handle IC hit path (no per-hit name compare, no property-count
  bound, no slot-attribute lookup for ordinary shaped data slots) are landed —
  **Richards interpreter `durationMs` 1818 → ~1515**, the ~20.2M `eq_str` name
  checks gone from the hit path. The 32-bit compressed slab rewrite, boxed heap
  doubles, fixed-inline + out-of-line overflow, elements/named split, and interned
  BaseShape are **still PENDING** (below).

`samply` on the interpreter Richards run after those changes:
- `dispatch_loop_inner` (the opcode match) dominates self-time — the inherent
  stable-Rust dispatch floor. Threaded-dispatch rewrite is retired (needs nightly
  `become`); this floor does not move without the optimizing tier.
- `HoltStack::index`/`index_mut` ≈ 3% — the register-window double indirection
  (audit area C). Removing it is the **P2** flat-stack work.
- `load_own_data_slot_atom` ≈ 0.9%, `LoadPropertyIc::load` ≈ 0.8% — the property
  path is no longer a hot spot.

Current interpreter thermometer (JIT off, Richards): `durationMs ≈ 1515`, Node 24
on the same loop ≈ 34 ms → **~44× node**. The interpreter is near its stable-Rust
floor; the remaining structural wins are P1 slab density (GC scan), P2 (flat stack +
`get_unchecked` regs + pre-decode + superinstructions), P4 (GC root scan), and the
deferred optimizing tier.

## Ground rules

- No new flags / `OTTER_*` toggles / `thread_local` / process-global caches. One
  default path; revert via git.
- **No fallback paths left behind.** When a representation is replaced, the old one
  is deleted, not gated. Dictionary mode is the *only* sanctioned slow path and it
  stays explicitly isolated.
- Preserve moving-GC + manual rooting + exact-PC deopt.
- oxc ASTs for JS/TS parsing; never regex-parse.

## Verification (every landed slice — non-negotiable, the aggression is in the code not the testing)

```bash
cargo build --release -p otter-cli
node benchmarks/diff.mjs                                   # 24/24 identical, all tiers
just test262 2>&1 | tee /tmp/after                         # FAILING-SET identical vs baseline
OTTER_GC_STRESS=128 ./target/release/otter run benchmarks/scripts/richards.js
OTTER_STATS=1 ./target/release/otter run benchmarks/scripts/richards.js   # thermometer
```

A bottleneck/win claim needs a counter + profile pair (`OTTER_STATS=1` +
`samply`/`atos`). The interpreter is the thermometer.

---

## P0 — Value reencoding to JSC pointer-cheap layout

**LANDED.** Pointers verbatim (top16 = 0, no unmask deref), doubles `+2^49` offset,
int32 tagged; registers hold full decompressed pointers, heap stays compressible
(P1). Bedrock for every later slab/guard/IC. See `VM_ABI_AUDIT.md` area A.

---

## P1 — Object model rewrite: flat shape→slot, 32-bit compressed slabs

**Aggression.** The god-struct split and shape-handle IC hit path are landed (see
Status). Remaining: rewrite `ObjectBody` storage so a property access is
`shape-id guard ⇒ decompress(slab[slot])` over **32-bit compressed slots**, with the
principled fixed-inline + overflow split, separate elements store, and interned
BaseShape.

**What ships (PENDING).**
1. **Slab keyed purely by shape.** Slot offset comes from the shape (`values_ptr`
   base already exists, `object.rs:493`). Fix the R2 cliff (`memory:
   engine_perf_audit`) so **overflow slots are equally IC-cacheable** — via the
   principled fixed-inline + out-of-line overflow split (item 3), where the IC
   handles "inline → 1 load, out-of-line → 2 loads." Delete
   `body_key_matches`/`eq_str` (`object.rs:~1660`, `string/gc_body.rs:612`) from the
   hit path entirely.
2. **32-bit compressed slabs + boxed heap doubles** (V8/Hermes). Object slots are
   **32-bit cage-relative compressed** values, not 8-byte `Value`s: slab density ×2,
   GC scan bytes ÷2. The slab edge gets a `compress(Value)->u32` /
   `decompress(u32)->Value` pair (decompress = `cage_base + offset` → full-pointer
   `Value` per P0; small-int inline via low-bit tag in the 32-bit slot; **f64
   doubles BOXED on the heap** as a `HeapNumber`-equivalent, referenced by compressed
   pointer). Property read = `shape-id guard ⇒ decompress(slab[slot])` (`slot*4`).
   `cage_base` pinned in a register across the interpreter loop. The IC caches the
   32-bit compressed shape offset as its key.
   **Sequence note:** slab layout and remset precision interact (a compressed slot is
   4 bytes → the SlotSet bit-density doubles, and `decompress` must not force a
   per-cell `type_tag` read on reference loads). Land this **alongside / after P4's
   slot-precise SlotSet** so the two are co-designed, not before it. `HeapNumber`
   boxing regresses reference-heavy loads unless the shape tracks per-slot
   representation (tagged/smi/double) — that shape-system change is part of this item,
   done together with P4.
3. **Fixed inline slots + out-of-line overflow — do NOT realloc the body**
   (research: V8 `inobject_properties`, JSC butterfly, SM `numFixedSlots`). Cap
   inline slots at construction; spill the rest to a separate growable array. A slab
   we `realloc` on every add relocates the object and breaks IC offset stability —
   replaces the `INLINE_VALUE_CAP` cliff with a *principled* boundary the IC handles
   as "inline → 1 load, out-of-line → 2 loads."
4. **One shape contract.** `AtomOwnPropertyHit` (`object.rs:425`) collapses to
   `{ shape_id, slot }` — no `atom_id`. Shape-id match is *sufficient*. Compare the
   **32-bit ShapeID as an immediate** (JSC StructureID), not a 64-bit pointer.
5. **Separate elements (integer-indexed) from named properties** into distinct
   backing stores (V8/JSC). Indexed writes must not thrash named-property shapes.
6. **Shared BaseShape** = interned `{class, realm, proto}` (SM/JSC) so proto/class
   stay out of the transition tree. Always advance the shape via define, **never
   dict-flip the prototype** (our known fast-shape-killing bug). Hot/cold split:
   exotic/optional fields → keyed side-table (Nova), not inline.

**Breaking.** `ObjectBody` slab layout, IC hit structs, dictionary transition path.
GC trace must walk the compressed slab + lazy `ExoticData`.

**Invariant.** `shape_id(recv) == baked ⇒ slot valid`. Name strings only on
miss/install. Dictionary mode (null shape) is the lone isolated slow path.

**Verify.** diff 24/24; full test262 `built-ins/Object`, `language/.../property-*`,
proxy, accessors failing-set identical; `OTTER_GC_STRESS=128`; Richards
`durationMs` + `propertyLoadHits` profile.
**Result gate:** slab GC scan bytes halved; no reference-load regression.

**Risk.** High (object model is everything). Hazard = shape-id reused with a
different slot map → silent wrong-slot read. Debug-only assert that resolved slot's
key equals requested key on every hit (compiled out of release) + the
attribute/accessor test262 suites are the net.

---

## P2 — One flat value stack + zero-copy frames + interpreter hot path

**Aggression.** Delete `Frame.registers: Vec<Value>`. ONE flat per-isolate `Value`
stack; frames are windows into it; interpreter and JIT share that single stack — no
parallel `reg_stack` (the `JitCtx.reg_stack_base`/`reg_top_ptr` duplication goes
away). Register read = one indirection `base+j*8`. Layout copied from the shipped
consensus (Hermes/JSC/Boa/BEAM, `VM_ENGINE_RESEARCH.md` §4). **Dispatch is NOT
rewritten — research retired that idea** (tail-call threading needs nightly
`become`/`preserve_none`, ~1–5% payoff; register-VM under a `match` already banks
Ertl's 1.48×). The win is the flat stack + zero-copy calls + stable-Rust hot-path
levers.

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
Richards `durationMs` + `samply` shows `HoltStack::index`/`index_mut` gone.
**Result gate:** ns/reduction drops materially (the register-deref tax).

**Risk.** High — touches every call path + generators + GC roots. Reservation-stable
must hold exactly (a realloc dangles live frame pointers). Sub-gate generators/
async on their own conformance run before merge.

---

## P3 — Thin `Result`, cold-error, hot-coercion sweep

**LANDED.** `Result<Value, VmError>` is pointer-sized (`VmError` `Copy`, ≤24 B, cold
payload boxed); success path never builds or drops error data; `NativeError::Coded
{code:&'static str}` kept for Node `ERR_*`; ToNumeric/ToPrimitive elision mined
(`memory: jit_design_plan`). No remaining work unless a profile says otherwise.

---

## P4 — Generational remembered-set GC (ship the algorithm, not just telemetry)

**Aggression.** Object-granular remembered set already landed (VM_GC_REDESIGN Step 1:
nbody `oldHeadersWalked` 14013→0, pause 221→142 μs). Remaining: make a **fully
slot-precise SlotSet** authoritative for old→young, add safepoint stack-map rooting,
the Smi-skip write barrier, and off-heap large blobs.

**What ships.** (research sharpens the original card-table plan — `VM_ENGINE_RESEARCH.md` §5)
1. GC telemetry counters (`heap.rs collect_minor_internal`, `scavenger.rs`): pause
   totals/max, root slots per category, dirty slots scanned, objects traced, young
   copied/promoted. Land first within the phase to prove each split.
2. **Precise SlotSet remembered set, NOT a card table** (V8 field-logging). 1 bit
   per pointer-slot, 1024-bit buckets, lazily allocated, ~3% overhead; minor GC
   iterates set bits → *exact* old→young slots. This **avoids the swept-corpse
   use-after-free bug class** the card-table dirty-walk produced
   (`scan_old_dirty_cards`) — no walking arbitrary headers on a dirty card. With
   P1's 32-bit compressed slots each entry is 4 bytes, bit-density doubles — co-design
   with P1 item 2.
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

## P5 — Cross-cutting: one IC IR (CacheIR) + work-proportional metering

Two research-driven additions that span the other phases (`VM_ENGINE_RESEARCH.md`
§3, §8). Both PENDING.

**5a — CacheIR-style single IC IR.** A **linear guard IR**
(`GuardShape Op0 Field0; LoadFixedSlotResult Op0 Field1`) with concrete
`Shape*`/slot-offset in per-stub **"stub data," not baked into the IR**. The
interpreter executes it via a tiny IR interpreter; the future optimizing JIT lowers
the *same* IR. SM's biggest lesson: never write interpreter ICs and JIT ICs twice
and keep them in sync — the exact divergence class that produced the JIT crash
(`VM_ABI_AUDIT.md` D), applied to ICs. Moving-GC synergy: constants in stub data
make IC roots enumerable/traceable (one updatable `Shape*` per stub). Replaces the
ad-hoc `property_ic.rs` / `method_ops.rs` IC structs with one representation. Land
with P1 as the IC substrate.
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
P0 (Value reencoding) ── LANDED (bedrock).
P3 (thin VmError)     ── LANDED.
P1 god-struct split + IC hit path ── LANDED.
   │
   ├─ P1 slab rewrite (32-bit compressed slabs + BaseShape + elements split)
   │     └─ co-designed with P4 SlotSet (slab layout ↔ remset precision)
   ├─ P2 (flat reg stack) ── P4 stack-maps depend on it
   ├─ P4 (GC slot-precise SlotSet + stack-map rooting)
   └─ P5 (5a IC IR with P1, 5b metering)
```

All VM-internal, JIT-measurable, independently shippable. Recommended sequence by
ROI/risk: **P2 → P1-slab + P4 (co-designed) + 5a → 5b**. P1's 32-bit slabs assume
P0's full-pointer registers + compress/decompress edge (landed); P4's stack-map
rooting needs P2's flat stack; the slab layout and SlotSet precision interact so
P1-slab and P4 are co-designed.

**Decisions — SETTLED 2026-06-29** (`VM_ENGINE_RESEARCH.md`):
1. **32-bit compressed object slabs + boxed heap doubles** — YES (P1 slab, co-designed
   with P4). Slab density ×2, GC scan bytes ÷2; doubles boxed on heap; shape tracks
   per-slot representation to avoid a reference-load `type_tag` regression.
2. **Flip to JSC pointer-cheap NaN-box** — DONE (P0). Pointers verbatim (top16=0,
   free deref); doubles `±2^49` offset; float box/unbox pays an ALU op (competitive
   on float benches — the conscious trade).
3. **precise + moving + stack-maps** — CONFIRMED (P4). Keep compaction +
   compression; cure the per-call rooting tax via safepoint stack-maps. JSC
   conservative+non-moving is NOT taken (it forfeits compaction + stack-reachable
   compression).

---

# Appendix — Deferred: JIT↔VM ABI unification (separate future project)

Audit areas D / E / F (`VM_ABI_AUDIT.md`) — the JIT↔VM ABI, plus the optimizing-tier
re-enable. **Out of scope for the VM phases above.** The four VM phases deliberately
preserve exact-PC deopt + reservation-stable frames + the shape→slot contract, so
this future effort needs no further VM change. The ABI types are DEFINED
(VM_GC_REDESIGN Section A: CacheStub version+snapshot, deopt+stack-map ABI,
field-repr ABI); this project wires them to codegen.

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
page layout is re-derived inside the JIT view and `debug_assert`'d against otter-gc.
Target: one published `JitGcView` (`heap_ptr`, `cage_base`, card-bitmap offset, page
mask, card shift, `FLAG_YOUNG`, GcHeader offsets) owned by otter-vm; pointer-reconstruct
+ card-mark + nursery-bump emit inline from it. `cage_base` also serves the value-side
pointer reconstruction (audit area A).

## Deferred 6 — Re-enable the optimizing tier

After D+E+F land, turn the tier on: the divergent hand-built tail no longer exists
(one `emit_enter_callee`), so the fid-27 crash class is structurally gone. Positive
proof at that time: Richards RC=0 + `richards=260000` with the tier on,
`jitDirectCalls` ≫ 260,060, diff 24/24, test262 `OTTER_JIT=1`==`OTTER_JIT=0`,
`OTTER_GC_STRESS=128 OTTER_JIT=1` + the deopt suite.
