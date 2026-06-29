# Engine Rework Tracking Plan

Fresh architecture/performance review tracking file for large structural work.
This plan intentionally excludes the separate "make Octane a correctness gate"
task. Keep steps large, independently shippable, and measured.

## Baseline Measurements

Measured on macOS arm64 with `target/release/otter`.

- `node benchmarks/diff.mjs`: 21/21 identical across `interp`, `jit`, `jit-osr`.
- Direct Richards loop, 500 `runRichards()` calls:
  - Node v24.16.0: 53 ms.
  - Bun v1.3.14: 66 ms.
  - Otter JIT: 12077.7 ms.
  - Otter interpreter: 11899.1 ms.
- Otter JIT stats for direct Richards loop, 500 calls:
  - `jitMethodGenericCalls=1626370`
  - `jitDirectCalls=0`
  - `reductionsExecuted=970109113`
  - `gcAllocBytesTotal=1011980528`
  - `gcCycles=62`
- `OTTER_JIT_TRACE=1` on short Richards run shows optimizing-tier declines on
  common real-workload opcodes including `LooseEqual`, `LooseNotEqual`, and
  `StoreElement`.
- `samply` profile on the 500-call Richards loop captured about 80k samples on
  `otter-isolate`. Use the profile only for attribution because profiling
  overhead increased wall time from about 12.1 s to about 27.3 s. `atos` resolved
  hot frames to `dispatch_loop_inner`, `HoltStack` indexing,
  `LoadPropertyIc::load`, `load_own_data_slot_atom`, and
  `jit_runtime_call_method`.

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
  - `node benchmarks/diff.mjs` remains 21/21.
  - Targeted Test262 for changed language semantics.
  - `OTTER_GC_STRESS=128 OTTER_JIT=1` on relevant workloads.
  - Real benchmark wall-clock against Node/Bun, with `OTTER_STATS=1`.

## Step 1: Polymorphic Property/Method ICs And Direct Calls

Status: in progress — baseline polymorphic method inline landed.

Landed: baseline JIT now bakes a most-frequent-first inline guard chain for
polymorphic `Op::CallMethodValue` sites (up to four distinct receiver
shapes) instead of collapsing to the per-call method bridge on the second
shape. `MethodCallFeedback::Poly` carries the observed targets;
`>4` distinct targets become `Megamorphic` and keep the bridge. A
polymorphic thermometer (four sibling classes sharing one call site) drops
from 3482 ms to 533 ms (the 23.97 M per-call method-bridge entries fall to
zero); monomorphic OO benches are unchanged. Verified `node
benchmarks/diff.mjs` 22/22 (new permanent gate `poly-dispatch.js`),
adversarial edge cases (megamorphic / own-vs-prototype method / throwing
method through the chain) identical across interp/jit/jit-osr, prototype
method-reassignment invalidation correct, and `OTTER_GC_STRESS=128
OTTER_JIT=1` checksums correct.

Remaining: megamorphic (`>4` shapes) still takes the full bridge; the
optimizing tier still bridges polymorphic method sites; polymorphic
property-load ICs and direct compiled-call entry for matched targets are not
yet done.

Goal: make real OO dispatch stop going through the generic method bridge.

Measured problem:

- Richards direct loop is about 228x slower than Node and about 183x slower
  than Bun.
- `jitMethodGenericCalls=1626370`, `jitDirectCalls=0`.
- JIT and interpreter are effectively tied on Richards, so current tiering is
  not paying for OO/polymorphic code.

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

- Richards direct loop:
  - `jitMethodGenericCalls` should drop materially.
  - `jitDirectCalls` should rise from zero.
  - Wall-clock should move by a large factor, not single-digit percent.
- `node benchmarks/diff.mjs` remains 21/21.
- Add targeted tests for polymorphic receiver shapes and prototype method
  replacement invalidation.
- Run `OTTER_GC_STRESS=128 OTTER_JIT=1` on Richards and focused IC tests.

## Step 2: Broaden Optimizing-Tier Coverage For Real OO Code

Status: in progress — `LooseEqual`/`LooseNotEqual` lowering landed (`9e4bef3f`).

Goal: make hot real-workload functions compile into the optimizing tier instead
of falling back to baseline/interpreter.

Measured problem:

- `OTTER_JIT_TRACE=1` on Richards shows optimizing-tier declines on
  `LooseEqual`, `LooseNotEqual`, and `StoreElement`.
- Current accepted subset is strong for numeric loops but poor for OO benchmark
  kernels.

Progress / findings:

- `LooseEqual`/`LooseNotEqual` now lower (nullish-literal identity test + numeric
  speculative path with deopt). Isolated loose-eq hot loop: interp 1.20s -> jit
  0.05s (24x). Richards loose-eq declines eliminated, but Richards wall is
  unchanged because those functions now decline on the NEXT unsupported op.
- The dominant remaining Richards blocker is `StoreProperty` of a POINTER value:
  the scheduler stores object references into fields everywhere
  (`this.list = currentTcb`, `this.link = link`, `this.currentTcb = new …`).
  The optimizing tier only stores primitive (int32/f64/bool) slot values today —
  a pointer store bails because there is no safepoint to run the generational
  write barrier. Lowering pointer `StoreProperty` (and `New`) therefore requires
  the write-barrier + register-map safepoint work that overlaps Step 3; do it as
  a dedicated GC-careful slice, not a rushed inline barrier.

Root cause anchors:

- `crates/otter-jit/src/optimizing/builder.rs`: bytecode to SSA coverage and
  unsupported-op decisions.
- `crates/otter-jit/src/optimizing/ir.rs`: node/repr surface.
- `crates/otter-jit/src/optimizing/clif/mod.rs`: Cranelift accepted subset.
- `crates/otter-jit/src/optimizing/emit.rs`: dynasm lowering, safepoints, deopt.
- `crates/otter-vm/src/lib.rs`: `compile_jit_function` bakes feedback into
  `JitFunctionView`.

Work:

- Add real lowering for `LooseEqual` and `LooseNotEqual` with speculative
  primitive/object paths and exact deopt.
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

- `OTTER_JIT_TRACE=1` should show Richards/DeltaBlue hot functions compiling
  instead of baseline fallback for the targeted opcode classes.
- Targeted Test262 for equality semantics, element access, object/array
  literals, and constructor behavior.
- `node benchmarks/diff.mjs` remains 21/21.
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
- `node benchmarks/diff.mjs` remains 21/21.

## Step 4: Make GC Remembered-Set Behavior Measurable, Then Reduce Root Work

Status: pending.

Goal: make GC costs visible enough to optimize safely, then reduce repeated
broad root scanning during minor collections.

Measured problem:

- Richards direct loop allocates about 1.01 GB and triggers 62 minor GCs.
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
- `node benchmarks/diff.mjs` remains 21/21.

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

