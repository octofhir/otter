# VM_GC_REDESIGN.md — Minor GC / remembered set / write barrier

**Status:** Before-JIT GC redesign complete (Steps 0–2 landed). Card table is
**dormant**, retained deliberately for the frozen-JIT write-barrier ABI (§Barrier)
— not deleted. Remaining work = Step 3 (conditional, gated on Step-0 counters) +
Step 4 (JIT-era). The JIT stays frozen/off; this file specifies the *VM ABI* the
re-enabled JIT lowers against, never JIT code.

**Gate:** `cargo test -p otter-vm` + `OTTER_GC_STRESS=32/64/128` + `diff.mjs`
(24/24) + per-bench interpreter timing.

---

## Landed ledger

| Step | Commit | Change | Measured win |
|------|--------|--------|--------------|
| **0 — telemetry** | `697078d9` | `ScavengeStats` counters (`minor_pause_ns`, `old_headers_walked`, `objects_retraced`, `slots_scanned`, `remset_entries`) + `Instant`-timed pause; no behavior change. | Baseline instrumentation for every later delta. |
| **1 — object-granular remembered set** | `ca04b7e8` | Replaced per-page card bitmap + full-page header walk with a per-isolate `Vec<RawGc>` store buffer of mutated old parents, deduped by `FLAG_REMEMBERED` (JSC/QuickJS model). Barrier still parent-header-granular; the ~78 `record_write` sites unchanged. | **Whole-class win:** nbody `old_headers_walked` 14013→0; minor pause 221→142μs. |
| **2 — body shrink** | `ed77b46b` | `INLINE_SLOT_CAP` 4→2; `ObjectBody` 96→88 B. Spill path + `refresh_values_ptr` are constant-driven, so the cap change is mechanically contained. | ObjectBody −8 B/object; reclaims the `item4` RSS slack. Inline slots kept (locality) — not reverted to zero. |

Card table is **dormant** after Step 1: no longer on the minor-GC path, retained
only as the ABI substrate for the future JIT inline barrier (§Barrier).

---

## Remaining forward work

### Step 3 — slot-range / SlotSet precision (CONDITIONAL)

Attacks **W2** (whole-object re-trace: a buffered old parent is re-traced in full
via `trace_slots_safe` even when one edge was dirtied). Add an SM-style
`SlotsEdge { owner: RawGc, start: u16, count: u16 }` store-buffer variant for the
string-keyed value slab; trace by re-reading the refreshed `values_ptr`. Keep the
object-granular whole-cell entry as the default and the **exotic-edge fallback**
(symbol props, accessor pairs, mapped-args cells — not addressable by
`(owner, slab_index)`).

**Gate before doing this:** only pursue if Step-0 counters show
`objects_retraced` / `slots_scanned` still dominate after Steps 1–2. Verify with a
`slots_scanned` delta proving W2 actually shrank. Do not start unmeasured.

### Step 4 — variable-size in-object bodies (JIT-ERA, deferred)

Size the inline region per hidden-class (V8 in-object props / JSC `inlineCapacity`
/ Hermes `DIRECT_PROPERTY_SLOTS`). `GcHeader.size_bytes` (`u32`) already strides
variable-size, so allocation-stride is free; the cost is (a) a manual DST /
thin-pointer body layout (Rust has no flexible array member — abandons the safe
`#[repr(C)]` derive) and (b) re-pinning every baked JIT offset (`shape@0`,
`values_ptr`, `jit_proto`) against a hand-laid prefix. Deferred to the **WITH-JIT
project**: unlocks on-heap overflow slab + fully slot-precise remembered set.
Undertake only if the off-page Vec + W2 still dominate after Step 3.

---

## Frozen-JIT write-barrier ABI contract

What this redesign promises the future optimizing tier.

**Byte-stable, must not change:** `OBJECT_BODY_SHAPE_OFFSET == 0`,
`OBJECT_BODY_VALUES_PTR_OFFSET`, `OBJECT_BODY_JIT_PROTO_OFFSET`; `CompressedValue`
tag constants; `Value` NaN-box; the `CacheStub`/`CacheOp` contract; deopt /
frame-state / register-window layout.

**The JIT-bakeable inline generational barrier** the re-enabled tier must emit
(2-instruction inline, strictly simpler than the old card-address arithmetic of
page-mask + `card/8` + `1<<(card%8)`):

```
write_barrier(parent_header, child):
  load parent.flags byte at HEADER_FLAGS_BYTE_OFFSET (=1)
  test FLAG_YOUNG (GENERATION_YOUNG_FLAG) on child
  if (parent.is_old ∧ child.is_young ∧ ¬FLAG_REMEMBERED):
      set FLAG_REMEMBERED; cold-call remembered_parents.push(parent_offset)
```

Baked constants the JIT must honor: `FLAG_REMEMBERED = 0x40`,
`GENERATION_YOUNG_FLAG` (`header.rs`), `HEADER_FLAGS_BYTE_OFFSET = 1`. Entries are
consumed each minor GC and `FLAG_REMEMBERED` cleared; full GC clears the whole
buffer + all remembered bits and rediscovers old→young via the full trace. The
remembered parent is **old** (non-moving on a minor GC), so its `RawGc` offset is
stable across the scavenge that consumes it. The dormant card table stays present
in the tree specifically so this barrier ABI has its substrate when the JIT
re-enables.
