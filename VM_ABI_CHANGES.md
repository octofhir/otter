# VM_ABI_CHANGES.md — Remaining JIT-Era ABI Work

**Status.** Section A — the frozen, JIT-bakeable core ABI (A1–A9) — is **landed**,
except **A3** which is deferred to `VM_GC_REDESIGN.md` (it needs variable-size GC
bodies). The remaining work is (1) wiring the already-*defined* A4/A5/A7 ABI types
into re-enabled optimizing-tier codegen, and (2) the Section B layout changes below,
which only pay off once that tier exists. **Live gate:** `cargo test -p otter-vm`
(657) + `OTTER_GC_STRESS=32/64/128` + the `diff.mjs` jit-vs-interp suite (24/24 —
active, not stale). ObjectBody is **88B** (reduced 96→88).

The correctness bar stands for all future work: moving-GC invariants (Cheney
young-gen, in-place 4-byte slot rewrite, `RawGc` cage compression), manual rooting,
and exact-PC deopt frame-state metadata must survive every change. Breaking changes
allowed — single binary, no back-compat.

---

## Landed ledger (Section A)

| # | Commit | Change |
|---|---|---|
| A1 | `34f82f34` | Object-body offset contract pinned + statically asserted |
| A2 | `33c8d8de` | `values_ptr` always-current base invariant, debug verifier |
| A3 | — | On-heap overflow-slab migration **deferred** (`VM_GC_REDESIGN.md`) |
| A4 | `b87637c7` | `CacheStub` versioned + copy-on-compile feedback snapshot |
| A5 | `1d68a1eb` | Monotone per-slot representation on the shape contract |
| A6 | `99551959` | `Value`/`CompressedValue` encoding constants frozen + asserted |
| A7 | `9f0222d3` | Exact-PC deopt frame-state + safepoint stack-map ABI (`deopt.rs`) |
| A8 | `9e9cce9e` | Windowed frame/stack header layout frozen |
| A9 | `f6cad987` | `GcHeader` flag bits + inline write-barrier ABI face frozen |

A4/A5/A7 define ABI *types* that the interpreter does not yet populate; the
re-enabled optimizing tier is what lowers and bakes against them.

---

# Section B — Do WITH JIT (list now, defer)

These only pay off once an optimizing tier exists. Recorded so the landed work does
not foreclose them; **not** done now.

| # | Deferred change | Pays off when | Engine lesson |
|---|---|---|---|
| B1 | Per-shape variable inline-slot count (drop fixed `INLINE_SLOT_CAP=4`) | JIT bakes per-shape offsets | V8 slack tracking, JSC `inlineCapacity`, Hermes `DIRECT_PROPERTY_SLOTS=5` |
| B2 | Move `jit_proto` onto the shape, reclaim 8B/object | JIT no longer needs per-object proto guard | QuickJS proto-on-shape, SM/JSC proto-on-structure |
| B3 | Megamorphic escape valve (global stub cache) for `CacheStub` | JIT lowers poly→megamorphic | V8 stub cache, JSC megamorphic |
| B4 | Fold Array/Collection builtin dispatch into CacheIR-guarded stubs | JIT wants one lowering surface | SM emits CacheIR for dense-element; V8 keeps it in typed-lowering |
| B5 | Unboxed-double inline slot (kill the `0b010` HeapNumber box) | repr-selection keeps doubles unboxed | JSC 8-byte inline slot |

**B1 — Per-shape inline-slot count.** `ObjectBody.inline_values` /
`INLINE_SLOT_CAP=4` (`object.rs:566`/`:574`). Every object pays for 4 inline slots;
there is no interp win from per-shape sizing because the interpreter reaches all
slots uniformly through `values_ptr`. The win is JIT-only (bake a per-shape inline
offset, skip the overflow indirection). Revisit alongside A1's size lock. Tension
with QuickJS's "zero inline slots, tiny body" RSS result — the right inline count is
a JIT-era tuning decision.

**B2 — `jit_proto` onto the shape.** `ObjectBody.jit_proto` @
`OBJECT_BODY_JIT_PROTO_OFFSET` exists so the method-inline guard reads the prototype
from the body in one load. With no JIT it is 8 dead bytes per object that
QuickJS/SM/JSC keep on the shared shape. Moving it now is churn A1 would have to
re-pin; weigh it when the JIT's method-inline guard is designed (it may prefer
chasing proto through the already-guarded shape).

**B3 — Megamorphic escape valve.** `CacheStub` (`cache_ir.rs:100`) runs a linear
operand-file program. V8/JSC degrade a megamorphic site to a hashed global stub
cache rather than a linear scan. Only matters once the JIT lowers polymorphic guard
chains. Defer.

**B4 — Builtin dispatch into CacheIR.** Array/Collection method ICs are a separate
native-fn-identity fast-dispatch, *outside* `CacheStub`. SM emits CacheIR even for
dense-element fast paths (one lowering surface); V8 specializes arrays in
typed-lowering, *not* the IC. Keep the split for now — idiomatic (JSC dispatches
array intrinsics by `indexingType`/callee identity too) and folding it in pre-JIT
buys nothing. Revisit when the JIT's lowering surface is concrete.

**B5 — Unboxed-double inline slot.** `CompressedValue` `0b010`
(`value/compressed.rs:34`) boxes any double/wide-int into a separate `HeapNumber`
cell, adding a load + a GC edge per numeric property — a cost JSC's 8-byte inline
slot never pays. Changing this trades away the 32-bit cage compression density and
only pays once A5's repr-selection keeps doubles unboxed in the JIT. Defer; if
double-heavy property workloads regress in the interpreter first, keep `HeapNumber`
allocation cheap and ensure storing an immediate over a boxed slot is
barrier-skipped (the existing smi-skip at store sites already does this).

---

## Cross-links

- **`VM_GC_REDESIGN.md`** — remembered-set redesign (**P4 precise object-granular
  set landed**), on-heap overflow-slab migration (the deferred A3), minor-GC scan
  rewrite, variable-size body support. Owns the GC mechanism behind A3 and A9.
- **`VM_JIT_FRIENDLY_REFACTOR_PLAN.md`** — P2 flat register stack (frozen by A8), P3
  thin `VmError` (frozen by A8), P4 remembered-set verdict (superseded by
  `VM_GC_REDESIGN.md`, now landed).
- **`VM_ABI_AUDIT.md`** — areas A–F audit this document acted on.
