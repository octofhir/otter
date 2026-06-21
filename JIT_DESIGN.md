# Otter JIT Design

**Updated:** 2026-06-21
**Branch:** `perf/engine-rewrite`
**Current head:** `19acb56d feat(jit): fast-path opt self recursion`

This document describes the current JIT architecture and the work that remains
to close Tier1. Historical phase notes were removed; completed implementation
details live in git history.

## Current Architecture

Otter runs a register bytecode interpreter with two compiled execution paths:

- **PupJIT baseline tier**: low-latency arm64 dynasm emitter over bytecode
  views. It handles broad arithmetic/control/object-call coverage and remains
  the fallback-friendly compiled tier.
- **DiveJIT optimizing tier**: typed SSA graph, liveness, linear-scan
  allocation, exact-PC deopt frame states, OSR entry, direct calls, monomorphic
  method inlining, and frameless self-recursive opt calls for safe graphs.

The interpreter remains the correctness oracle and deopt target. `OTTER_JIT=0`
disables compiled execution for differential checks; this is a test/runtime
control, not a development kill-switch for individual features.

## Landed Tier1 Capabilities

Current Tier1 implementation includes:

- Function-entry tier-up and loop OSR.
- Exact OSR entry for hot loop headers.
- Typed SSA graph construction for hot supported regions.
- Trivial-phi elimination, including deopt frame-state and register-write
  consistency.
- Float and int numeric loop lowering with exact deopt.
- Direct optimized calls.
- `CallMethodValue` support in opt tier.
- Monomorphic tiny method inlining guarded by method identity.
- Frameless optimized self-recursive calls for eligible graphs.
- Inline property load/store and method feedback paths in the baseline JIT.
- Typed-array/numeric benchmark coverage sufficient for current Tier1 targets.

The latest verified release baseline is tracked in [`plan.md`](plan.md) and
[`OTTER_ENGINE_REFACTOR_PROGRESS.md`](OTTER_ENGINE_REFACTOR_PROGRESS.md).

## Current Performance Shape

Numeric/OSR-heavy Tier1 benchmarks are no longer the blocker:

- `mandelbrot.js`: closed against Node on the current machine.
- `nbody.js`, `fib.js`, `typed-array.js`: close enough for Tier1; do not trade
  regressions here for wins elsewhere.

Open Tier1 gaps:

- `array-ops.js` and `sort.js`: callback/comparator-heavy builtin paths.
- `prop-access.js`: residual object/method fallback or overhead.
- `string-ops.js`: string runtime/builtin throughput.
- `json.js`: JSON parser/stringifier allocation/traversal throughput.
- `regex.js`: regex engine throughput.

The next JIT-relevant work is callback/comparator and residual object/method
overhead. String, JSON, and regex may be runtime subsystem work rather than
compiler work, but they remain Tier1 closure items until explicitly assigned.

## Core Invariants

### Deopt

Every speculative guard must carry an exact frame state:

- bytecode PC to resume at;
- live interpreter register mapping;
- physical value location or constant;
- representation-aware boxing/reload.

No bail-to-entry shortcuts. OSR and nested-loop environments must reconstruct
the same interpreter-visible state as the uncompiled path.

### GC

Compiled code must not hide GC pointers from the moving collector.

- Unboxed `Int32` and `Float64` values can live in machine registers across
  non-GC operations.
- Boxed or heap-pointer `Value`s must be in traced frame/register roots at
  safepoints unless a precise stack map exists for that safepoint.
- Store fast paths must emit or call the correct write barrier.
- Deopt exits must materialize rooted frame slots before returning to the VM.

### Calls

Compiled call fast paths must preserve:

- stack-depth and sync-reentry checks;
- `this` coercion and bound-function behavior;
- upvalue/root liveness;
- throws and bailout propagation;
- frame/register window reclamation on every exit path.

Frameless self-recursive calls are only valid for graphs whose lowered body does
not need a real VM frame for runtime stubs or GC-visible frame metadata.

### Inline Caches

IC fast paths are valid only under complete guards:

- receiver tag/type;
- shape/prototype/method identity as required by the feedback;
- slot offset or element layout;
- dictionary/accessor/proxy fallback;
- write barrier for stores.

Megamorphic or semantically unstable sites must fall back cleanly.

## Remaining Tier1 Design Work

### Callback and comparator hot paths

Targets: `array-ops.js`, `sort.js`.

The prepared lean callback path already exists. Do not assume callbacks are
cold. Measure residual cost around:

- frame register reset;
- argument rebinding;
- `this` handling;
- compiled frame entry/exit;
- numeric comparator result coercion;
- dense output writes for `map` / `filter`;
- bailout frequency.

Only cut measured overhead. Generic Array semantics remain mandatory: holes,
inherited indices, accessors, species, callback side effects, exceptions, and GC
movement must all stay correct.

### Object and method residual overhead

Target: `prop-access.js`.

The current benchmark has monomorphic class instances and tiny methods. The
remaining task is to explain every fallback in the hot region and patch only the
measured misses:

- object-array dense element load;
- inlined method property load/store;
- method identity guard;
- write barrier path for `tag` updates.

No object shortcut may skip accessors, prototype changes, dictionary mode,
proxies, or barriers.

### Runtime-heavy Tier1 gaps

Targets: `string-ops.js`, `json.js`, `regex.js`.

These are Tier1 closure items, but not necessarily compiler features:

- string: measure build, split/join, `charCodeAt`, slice/indexOf separately;
- JSON: measure parse vs stringify separately and avoid known dict-mode
  fast-shape regressions;
- regex: measure match/replace/exec separately and preserve `lastIndex`,
  captures, replacement, Unicode, and case-folding semantics.

## Verification Contract

Each JIT/runtime slice must pass the checks appropriate to the touched surface:

- `cargo build --release -p otter-cli`
- `cargo test -p otter-jit`
- `cargo test -p otter-vm` when VM/runtime behavior changed
- full `benchmarks/scripts/*.js` output parity with `OTTER_JIT=0` and
  `OTTER_JIT=1`
- no JIT-on regression against the previous committed baseline
- `OTTER_GC_STRESS=128 OTTER_JIT=1` for touched workloads
- targeted Test262 subsets for touched builtins/opcodes with JIT on/off
  failing-set parity
- debug marker grep before commit:
  `CALLDBG`, `METHODDBG`, `OTTER_TMP_METHOD_FALLBACK`, `NODISABLE`,
  `OSRDBG`, `PREOSR`, `WBAIL`, `OSROUT`

## Non-Goals Until Tier1 Closes

- Broad peak-optimizer work.
- Feature flags or env kill-switches for unfinished behavior.
- Benchmark-specific semantic shortcuts.
- Changes that speed up an open benchmark while regressing `mandelbrot`,
  `nbody`, `fib`, or `typed-array`.

## Source Map

Primary files for the remaining Tier1 work:

- `crates/otter-jit/src/baseline.rs`
- `crates/otter-jit/src/optimizing/{builder.rs,deopt.rs,emit.rs,ir.rs,liveness.rs,regalloc.rs}`
- `crates/otter-vm/src/call_ops.rs`
- `crates/otter-vm/src/array_prototype.rs`
- `crates/otter-vm/src/method_ops.rs`
- `crates/otter-vm/src/property_ic.rs`
- `crates/otter-vm/src/object.rs`
- `crates/otter-vm/src/string/`
- `crates/otter-vm/src/regexp_prototype.rs`

Keep this document current when the active Tier1 queue changes.
