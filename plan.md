# OtterJS Tier1 JIT Closure Plan

**Updated:** 2026-06-21  
**Branch:** `perf/engine-rewrite`  
**Current head:** `19acb56d feat(jit): fast-path opt self recursion`

This file tracks only the work still required to close Tier1. Completed slices
live in git history and are intentionally not repeated here.

## Current Baseline

Release binary: `target/release/otter` from `otter-cli`.

Measured on 2026-06-21 with `OTTER_JIT=1`; Node measured with the same scripts.

| script | Otter JIT | Node | gap | Tier1 status |
|---|---:|---:|---:|---|
| `mandelbrot.js` | 0.03s | 0.03s | closed | no action unless regression |
| `nbody.js` | 0.07s | 0.03s | small | no Tier1 blocker |
| `fib.js` | 0.09s | 0.04s | small | no Tier1 blocker |
| `typed-array.js` | 0.09s | 0.03s | small | no Tier1 blocker |
| `prop-access.js` | 0.23s | 0.03s | open | finish object/method fast paths |
| `array-ops.js` | 0.45s | 0.09s | open | callback-heavy builtin path |
| `sort.js` | 1.12s | 0.16s | open | comparator-heavy builtin path |
| `json.js` | 1.42s | 0.23s | open | runtime/builtin throughput |
| `string-ops.js` | 0.44s | 0.03s | open | string builtin throughput |
| `regex.js` | 2.20s | 0.03s | open | regex engine throughput |

Numeric opt-JIT and OSR are no longer the Tier1 blocker. The remaining Tier1
work is concentrated in builtin/runtime call surfaces and residual object /
callback overhead.

## Remaining Work

### 1. Callback and comparator hot paths

Targets: `array-ops.js`, `sort.js`.

Goal: reduce the native-builtin to JS-callback floor enough that callback-heavy
benches are no longer the largest Tier1 JIT gap.

Work:

- Add focused counters around `array_callback_native_dispatch`,
  `native_sort` comparator calls, `run_bytecode_callable_committed_lean_args`,
  `invoke_prepared_lean`, and compiled-frame bail/return outcomes.
- Identify the remaining per-element cost after the prepared lean frame reuse:
  register reset, argument rebinding, `this` handling, compiled entry, result
  coercion, dense output writes, or sort comparator plumbing.
- Cut only measured overhead. Expected candidates:
  - narrower reset for callback frames whose live register set is known;
  - cached boxed index values where spec-safe;
  - tighter dense-array output creation for `map` / `filter`;
  - comparator fast return-to-number path for monomorphic numeric comparators.
- Keep generic semantics intact: holes, inherited indices, accessors, species,
  callback side effects, detached buffers, throws, and GC movement.

Gates:

- `array-ops.js` and `sort.js` output parity with `OTTER_JIT=0/1`.
- `OTTER_GC_STRESS=128 OTTER_JIT=1` on array callback and sort workloads.
- Targeted Test262 for `Array.prototype.{map,filter,forEach,reduce,sort,toSorted}`
  with JIT on/off failing-set parity.

### 2. Object and method residual fast paths

Target: `prop-access.js`.

Goal: make the hot loop stay on inline IC / inlined method paths with minimal
Rust stub traffic.

Work:

- Verify the hot loop graph for `Point.prototype.bump`, `dist2`, `p.x`, `p.y`,
  `p.tag`, and `pts[i]`; every fallback call in the hot region must be explained.
- Extend opt lowering only where a measured fallback remains:
  - dense array element load for object arrays if `pts[i]` is still a stub;
  - property store/load in inlined method bodies if any site misses the inline IC;
  - method identity guards only where the receiver/prototype chain feedback is
    monomorphic and stable.
- Preserve deopt exactness and write barriers. No stub bypass may skip accessor,
  prototype, dictionary, proxy, or GC semantics.

Gates:

- `prop-access.js` output parity with `OTTER_JIT=0/1`.
- `OTTER_GC_STRESS=128 OTTER_JIT=1 benchmarks/scripts/prop-access.js`.
- Relevant object/property/method Test262 subsets with JIT on/off parity.

### 3. String builtin throughput

Target: `string-ops.js`.

Goal: remove the largest non-regex runtime gap that still affects common JS
workloads.

Work:

- Measure `+=` string building, `split`, `join`, `charCodeAt`, `slice`, and
  `indexOf` separately.
- Inline or specialize only stable builtin fast paths:
  - one-byte / UTF-16 code-unit reads for `charCodeAt`;
  - substring/slice without avoidable copy where representation permits;
  - split/join fast paths for simple string separators;
  - string builder path that avoids quadratic copying.
- Keep Unicode and observable coercion behavior correct.

Gates:

- `string-ops.js` parity with `OTTER_JIT=0/1`.
- String builtins Test262 subsets for touched methods.
- GC stress on string build/split/join workloads.

### 4. JSON throughput

Target: `json.js`.

Goal: reduce parse/stringify runtime cost without weakening correctness.

Work:

- Measure parse vs stringify separately.
- Keep previous regression lesson: no dict-mode fast-shape shortcut unless it is
  proven by conformance and perf.
- Prioritize allocation and object traversal costs that are visible in both
  JIT modes.

Gates:

- `json.js` parity with `OTTER_JIT=0/1`.
- JSON Test262 subsets.
- GC stress on repeated parse/stringify.

### 5. Regex throughput

Target: `regex.js`.

Goal: stop regex from dominating the Tier1 dashboard.

Work:

- Profile `match`, `replace`, and `exec` separately.
- Improve the existing regex engine or introduce the planned regex subsystem
  only as a contained slice with compatibility gates.
- Preserve `lastIndex`, captures, global/sticky behavior, replacement semantics,
  and Unicode/case-insensitive semantics.

Gates:

- `regex.js` parity with `OTTER_JIT=0/1`.
- RegExp Test262 subsets.
- GC stress on match/replace/exec loops.

## Tier1 Exit Criteria

Tier1 is closed when all of the following are true:

- `benchmarks/scripts/*.js` output-identical with `OTTER_JIT=0` and `OTTER_JIT=1`.
- No benchmark has a new JIT-on regression against the previous committed
  baseline.
- Numeric/OSR benches stay closed: `mandelbrot`, `nbody`, `fib`, `typed-array`.
- Remaining runtime-heavy benches have a documented owner if they cannot be
  closed inside Tier1 without starting a larger subsystem.
- `cargo test -p otter-vm` and `cargo test -p otter-jit` pass.
- Touched Test262 subsets have JIT on/off failing-set parity.
- `OTTER_GC_STRESS=128` passes for every touched workload.
- No debug prints, no env kill-switches, no benchmark-only shortcuts.

## Do Not Start Before Tier1 Exit

- Broad new peak optimizer work.
- New feature flags or env kill-switches.
- Rewriting unrelated runtime surfaces to chase one benchmark.
- Shipping a speedup that regresses any already-closed Tier1 benchmark.
