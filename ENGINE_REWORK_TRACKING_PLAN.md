# Engine Rework Tracking Plan

Live roadmap for the JIT optimizing tier. Keep steps large, independently
shippable, and measured. This plan intentionally excludes the separate
"make Octane a correctness gate" task.

## Current baseline

Measured on macOS arm64 with `target/release/otter`, baseline JIT on
(`OTTER_JIT=1`); min wall-clock via `node benchmarks/bench.mjs` (Node/Bun refs),
correctness gate `node benchmarks/diff.mjs` 24/24 identical across
`interp`/`jit`/`jit-osr`. The JIT value+slot ABI is fully ported to the JSC
pointer-cheap encoding.

Slower than node (× = factor): richards 44×, tree-traversal 42×, poly-dispatch
8.5×, nbody 7.7×, string-ops 7×, regex-heavy 6×, object-shapes 4×, map-set 3.5×,
prop-access 3.4×, json 2.6×. Faster than node: loose-eq 0.61×, regex 0.57×,
mandelbrot 0.73×, array-ops/control-flow 0.92×. The wall is method-heavy/poly
workloads (richards/tree/poly): baseline JIT is interp-bound there, so the fix
is the optimizing tier delivering un-inlined polymorphic `CallMethod` plus a real
compiled-call ABI.

## Rules

- No benchmark-fitting. Use real workloads and standard suites as yardsticks.
- No feature flags, environment kill-switches, process-global caches, or
  thread-local runtime caches.
- No per-call extern-C bridge stubs as the hot-path shortcut. Emit guards and
  direct paths inline.
- Keep the active runtime stack only: `otter-gc -> otter-vm -> otter-runtime`.
- Preserve exact-PC deopt, real register allocation, representation selection,
  speculative inlining, and precise GC/rooting invariants.
- Verification for every landed slice must include:
  - `node benchmarks/diff.mjs` remains 24/24.
  - Targeted Test262 for changed language semantics.
  - `OTTER_GC_STRESS=128 OTTER_JIT=1` on relevant workloads.
  - Real benchmark wall-clock against Node/Bun, with `OTTER_STATS=1`.

## Step 1: Polymorphic Property/Method ICs And Direct Calls

Status: in progress — baseline polymorphic method inline landed.

Landed: baseline JIT bakes a most-frequent-first inline guard chain for
polymorphic `Op::CallMethodValue` sites (up to four receiver shapes) instead of
collapsing to the per-call method bridge; `MethodCallFeedback::Poly` carries the
targets, `>4` becomes `Megamorphic` and keeps the bridge. Gated by
`poly-dispatch.js` in `diff.mjs`.

Remaining: megamorphic (`>4` shapes) still takes the full bridge; the optimizing
tier still bridges polymorphic method sites; polymorphic property-load ICs and
direct compiled-call entry for matched targets are not yet done.

### Profiling-confirmed blocker chain (2026-07-01)

A `sample` profile of the richards direct loop (`otter-isolate` thread) plus
`OTTER_JIT_TRACE=1` decline reasons pinned exactly why the optimizing tier does
not cover richards' hot method bodies, in order:

1. **`Unlowered("guard without deopt frame state")` — FIXED (`c5c35191`).** The
   value-ABI port made `LoadSlot`/`StoreSlot` deopt-capable (compress/decompress
   guard) but `deopt::can_deopt` did not list them, so every slot-touching body
   aborted. Added them; fids 26/32 now compile in the optimizing tier.
2. **`Opcode(StoreProperty)` / `Opcode(LoadProperty)` — the current wall (7×/1× on
   richards).** `Interpreter::bake_property_feedback` (`lib.rs:3056`) only bakes
   `property_feedback: Some((shape,slot))` when the IC site has **exactly one**
   entry (`[stub]`). Richards' `this.<field> = …` stores see several sibling task
   shapes, so the site is polymorphic → feedback `None` → `builder.rs` (LoadProperty
   ~842, StoreProperty ~895) declines. Mono-speculating one shape would deopt-storm
   a balanced poly site, so the fix is a real polymorphic IC, not first-shape
   speculation.

### Next slice — polymorphic property IC (Maglev `PolymorphicAccessInfo` shape)

Lower a poly property site as an **inline shape-guard chain in machine code** with
a deopt fallback — mirroring the existing inline collection-method IC
(`emit.rs::emit_opt_live_collection_leaf_method_guarded_call`), NOT synthetic SSA
blocks (no phi surgery: a load writes its one frame register on the matching arm; a
store has no result):

- `jit.rs`: add `property_feedback_poly: SmallVec<[(u32,u32); 4]>` to the view
  instruction (empty for mono/non-property). Init at the ~3 construction sites
  (`executable.rs:260`, `lib.rs:11160`, `lib.rs:16089`) + the synthetic ones in
  `optimizing/builder.rs`.
- `lib.rs::bake_property_feedback`: when the site has 2..=4 own-data-hit entries,
  collect each `(shape.offset(), slot*sizeof(CompressedValue))` into the poly list
  (keep the single-entry mono path as-is).
- `optimizing/ir.rs`: add `LoadSlotPoly(obj, Box<[(u32,u32)]>)` and
  `StoreSlotPoly(obj, Box<[(u32,u32)]>, value)`; wire `inputs()`/`repr()`
  (`LoadSlotPoly` → Tagged; `StoreSlotPoly` → no result) and add both to
  `deopt::can_deopt` (they bail before any write, like the mono forms).
- `optimizing/builder.rs`: at LoadProperty/StoreProperty, when mono
  `property_feedback` is absent but `property_feedback_poly` is non-empty and
  `cage_base != 0`, emit the poly node instead of declining.
- `optimizing/emit.rs`: lower the poly node — for each candidate: decompress the
  receiver (`cage_base + low32`), read `object_shape_byte`, compare to the
  candidate shape; on match run the existing mono `LoadSlot`/`StoreSlot` body
  (decompress/compress + slab access + card-mark for a Tagged store) and branch to
  `done`; on miss fall to the next candidate; the final miss takes the node's deopt
  exit. `clif/mod.rs` declines the poly node (property stores already route to
  dynasm).
- Verify: `diff.mjs` 24/24, `OTTER_GC_STRESS=128` on richards/tree/object-shapes,
  `OTTER_JIT=1 just test262` property suites failing-set == JIT-off, then the
  richards `StoreProperty` declines should fall and the hot task bodies compile.
  After this, the residual richards walls are `Opcode(New)` (constructor inline)
  and the poly `CallMethod` bridge (Step 2).

Goal: make real OO dispatch stop going through the generic method bridge.

Root cause anchors:

- `crates/otter-jit/src/baseline.rs`: `jit_call_method_stub_optimizing`.
- `crates/otter-vm/src/method_ops.rs`: `jit_runtime_call_method`.
- `crates/otter-vm/src/property_ic.rs`: current load/store ICs are
  monomorphic/direct-prototype oriented.
- `crates/otter-vm/src/lib.rs`: method-target feedback is recorded around
  `Op::CallMethodValue`, but the hot path still falls back to generic runtime
  method calls.

Work:

- Add shape-vector PIC metadata for property and method sites.
- Record receiver shape/prototype-shape/method-slot/callee-fid feedback per
  site.
- Emit inline guards for common polymorphic method targets.
- Enter compiled bytecode callees directly when guard and target match.
- Preserve fallback to existing generic path only on guard miss or unsupported
  callable shape.
- Keep all IC state isolate-owned and GC-traced; no process-global caches.

Verification:

- Richards direct loop: `jitMethodGenericCalls` drops materially,
  `jitDirectCalls` rises from zero, wall-clock moves by a large factor.
- `node benchmarks/diff.mjs` remains 24/24.
- Add targeted tests for polymorphic receiver shapes and prototype method
  replacement invalidation.
- Run `OTTER_GC_STRESS=128 OTTER_JIT=1` on Richards and focused IC tests.

## Step 2: Broaden Optimizing-Tier Coverage For Real OO Code

Status: in progress — `LooseEqual`/`LooseNotEqual` (`9e4bef3f`) and pointer
`StoreProperty` + inline generational write barrier (`3cf18d0f`) landed.

Remaining: opt-tier polymorphic `CallMethod` via a synthetic-block guard chain
(one guard/inline per candidate, miss → next, last miss → bridge/deopt, merge phi
for the dst register — extend `cfg.succs/preds/block_of_pc` and `graph.blocks`
mid-construction; anchors `optimizing/builder.rs` ~1073 mono `inline_methods`,
`optimizing/emit.rs` ~3105 `CallMethod` bridge, `optimizing_call_method_safepoint_id`).
Then `StoreElement` object-valued (array-ops, object-shapes), object/array
literal allocation, and `New`/constructor inline — each a GC-careful
alloc/safepoint slice, miscompile-sensitive (verify diff 24/24 + GC_STRESS=128 +
per-shape correctness).

Inline card-mark write-barrier recipe (dynasm `StoreSlot`, Tagged value; landed
reference for future pointer-store slices): after the slot store, (1) value
pointer-tag test `(top16 - 0x7FFC) <= 3` else skip; (2) parent young?
`[parent_hdr+1] & FLAG_YOUNG(0b100)` set → skip; (3) child young?
`child_hdr = cage_base + low32(value)`, `[child_hdr+1] & FLAG_YOUNG` clear → skip;
(4) `page_base = parent_hdr & ~(PAGE_SIZE-1)` (256KiB); `byte_off = parent_hdr -
page_base`; `card = byte_off >> 9` (CARD_SIZE 512); set bit `card&7` in byte
`page_base + CARD_BITMAP_OFF + (card>>3)`. `parent_hdr = cage_base +
low32(parent_tagged)`; PageHeader sits at page_base (`Page::page_base_of`). Bake
into `JitFunctionView` from otter-vm: `offset_of!(PageHeader, card_bitmap)`,
`!(PAGE_SIZE-1)`, card-size shift, and expose `FLAG_YOUNG` from otter-gc. In
Phase-1 the insertion (marking) barrier is DORMANT (STW marker), so only the
generational card-mark is needed — allocation-free, never moves GC, no
safepoint/frame-state required. clif does not lower StoreSlot (property-store
functions route to dynasm), so only emit.rs + builder.rs + the view change are
needed. trybuild `native_ctx_is_not_send.stderr` may need regen after the view
field is added.

Root cause anchors:

- `crates/otter-jit/src/optimizing/builder.rs`: bytecode to SSA coverage and
  unsupported-op decisions.
- `crates/otter-jit/src/optimizing/ir.rs`: node/repr surface.
- `crates/otter-jit/src/optimizing/clif/mod.rs`: Cranelift accepted subset.
- `crates/otter-jit/src/optimizing/emit.rs`: dynasm lowering, safepoints, deopt.
- `crates/otter-vm/src/lib.rs`: `compile_jit_function` bakes feedback into
  `JitFunctionView`.

Work:

- Add opt-tier polymorphic `CallMethod` lowering via the synthetic-block guard
  chain with exact safepoint/deopt.
- Add `StoreElement` coverage for dense arrays and common object-valued element
  cases.
- Add object/array literal allocation paths only with correct safepoints and
  deopt metadata.
- Add common object slot load/store paths based on baked shape feedback.
- Add constructor/new paths that can inline obvious JS constructors without
  violating `new.target`, `this`, derived constructor, or prototype semantics.
- Keep Cranelift and dynasm backend support explicit; if one backend declines,
  the other must still preserve semantics or decline cleanly.

Verification:

- `OTTER_JIT_TRACE=1` shows Richards/DeltaBlue hot functions compiling instead
  of baseline fallback for the targeted opcode classes.
- Targeted Test262 for equality semantics, element access, object/array
  literals, and constructor behavior.
- `node benchmarks/diff.mjs` remains 24/24.
- Compare wall-clock on Richards and DeltaBlue once correctness is available.

## Step 3: Real Compiled-Call ABI, Register Maps, And Exact Safepoints

Status: pending.

Goal: remove the boxed register-window frame rebuild / generic bridge ceiling
from hot compiled calls and allocating compiled operations.

Measured problem:

- Optimized code still pays through VM bridges at method/call boundaries.
- Safepoints currently use frame-slot windows for allocating compiled paths,
  forcing materialization around operations that should remain register based.

Root cause anchors:

- `crates/otter-jit/src/baseline.rs`: shared `JitCtx`, compiled entry ABI,
  runtime call stubs.
- `crates/otter-jit/src/optimizing/emit.rs`: frame-window ABI and safepoint
  model.
- `crates/otter-jit/src/optimizing/deopt.rs`: frame-state capture.
- `crates/otter-vm/src/lib.rs`: `SafepointRecord::frame_slot_window` baking.
- `crates/otter-vm/src/runtime_stubs.rs`: allocation and leaf stubs.

Work:

- Define a compiled-call ABI with register arguments/results for JS-to-JS calls.
- Preserve exact-PC deopt by carrying frame states at every call and guard site.
- Add register-map safepoints for allocating stubs.
- Keep frame-window materialization only for deopt/fallback paths, not the
  normal hot call path.
- Make recursive and polymorphic compiled calls enter compiled code directly
  when guards match.
- Avoid per-call extern-C bridge stubs as the hot path.

Verification:

- Recursive and call-heavy benchmarks show `jitDirectCalls` rising and bridge
  counters falling.
- `OTTER_GC_STRESS=128 OTTER_JIT=1` over call-heavy and allocation-heavy cases.
- Targeted deopt tests: guard miss after side effects, exception across compiled
  call, GC during allocating compiled op, recursive compiled calls.
- `node benchmarks/diff.mjs` remains 24/24.

## Step 4: Make GC Remembered-Set Behavior Measurable, Then Reduce Root Work

Status: pending.

Goal: make GC costs visible enough to optimize safely, then reduce repeated
broad root scanning during minor collections.

Measured problem:

- The Richards direct loop allocates ~1 GB and triggers dozens of minor GCs.
- Current minor GC combines handle stack, global handles, caller roots,
  extra roots, and frame roots each cycle.
- Dirty-card infrastructure exists, but root scanning is still broad.

Root cause anchors:

- `crates/otter-gc/src/heap.rs`: `collect_minor_internal`.
- `crates/otter-gc/src/scavenger.rs`: root pass, dirty-card pass, Cheney scan.
- `crates/otter-gc/src/barrier.rs`: write barrier and card marking.
- `crates/otter-vm/src/runtime_state.rs`: VM extra roots.
- `crates/otter-gc/src/frame_roots.rs`: frame root providers.

Work:

- Add GC telemetry counters:
  - minor/full pause totals and max pause,
  - root slots visited by category,
  - dirty cards scanned,
  - objects traced from dirty cards,
  - young bytes copied/promoted,
  - weak/ephemeron slots visited.
- Use telemetry to prove where pause time goes before changing algorithms.
- Make remembered-set/card scanning authoritative for old-to-young edges.
- Reduce repeated scanning of stable roots where a safe per-isolate root-set
  summary can be maintained.
- Preserve moving-GC safety and manual rooting invariants.

Verification:

- GC invariant tests and `OTTER_GC_STRESS=128`.
- Allocation-heavy real workloads and synthetic allocation loops with before/after
  pause totals.
- No increase in use-after-move failures under stress.
- `node benchmarks/diff.mjs` remains 24/24.

## Step 5: Standardized Benchmark Telemetry And Profile Discipline

Status: pending.

Goal: keep future performance work measured against real workloads with
actionable counters and line-confirmed profiles.

Measured problem:

- Synthetic scripts are useful parity smoke tests but misleading as primary
  thermometers.
- `Cargo.toml` currently warns that `profile.*.force-frame-pointers` manifest
  keys are unused, so profiling claims need explicit validation.
- `samply` profiles may be unsymbolicated until addresses are resolved with
  `atos` and the correct Mach-O base.

Work:

- Keep `benchmarks/scripts/*` as smoke/parity coverage, not the primary ROI
  metric.
- Standardize run output for real benchmarks:
  - wall-clock,
  - `OTTER_STATS=1`,
  - JIT trace summary,
  - GC telemetry summary,
  - Node/Bun comparison.
- Fix or document profiling profile settings so line-level attribution is
  repeatable.
- Add a small script or doc snippet for `samply` + `atos` address resolution on
  macOS.
- Require a profile/counter pair before accepting a bottleneck claim.

Verification:

- One-command reproduction for Richards direct loop and other real workloads.
- Saved benchmark artifacts include versions, host, command line, counters, and
  timing.
- Profile notes include whether samples were symbolicated directly or resolved
  with `atos`.
