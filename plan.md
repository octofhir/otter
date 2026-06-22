# OtterJS Tier1 JIT Closure Plan

**Updated:** 2026-06-22
**Branch:** `main`
**Current head:** `e2217376 perf(jit): inline dense array stores`

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
| `prop-access.js` | 0.17s | 0.03s | open | finish residual object/method cost |
| `array-ops.js` | 0.45s | 0.09s | open | callback-heavy builtin path |
| `sort.js` | 0.29–0.53s | 0.16s | open | fill-loop store stubs closed; callback body unsigned shift faster |
| `json.js` | 1.42s | 0.23s | open | runtime/builtin throughput |
| `string-ops.js` | 0.44s | 0.03s | open | string builtin throughput |
| `regex.js` | 2.20s | 0.03s | open | regex engine throughput |

Numeric opt-JIT and OSR are no longer the Tier1 blocker. The remaining Tier1
work is concentrated in builtin/runtime call surfaces and residual object /
callback overhead.

## Architecture Scope

Current Tier1 measurements and hand-emitted machine code are focused on arm64.
That is a sequencing choice, not a product constraint. Otter must support the
required deployment architectures after Tier1:

- aarch64 macOS/Linux;
- x86_64 Linux/macOS;
- Windows x86_64 once the runtime/JIT integration is portable enough.

The post-Tier1 backend plan is Cranelift-backed Tier2/DeepDive codegen with
Otter-owned deopt metadata and StoneMaps safepoint metadata. The current dynasm
arm64 backend remains the Tier1 path until Cranelift is correct, faster, and
covered by the same parity/GC/Test262 gates.

Reference engines are allowed and expected for architecture decisions. Node/V8
is the primary comparison point for multi-architecture release practice,
tiering, deopt metadata, safepoints, IC dependency invalidation, and benchmark
methodology. Use it as a reference, not as a naming source or a reason to bypass
Otter's own GC/deopt invariants.

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

Current `sort.js` status: bounded `new Array(n)` construction now materializes
dense hole storage for moderate lengths, and Tier1 dense-array `StoreElement`
inlines the fill loop when the array-index accessor protector and array exotic
state prove the write is not observable. Full benchmark JIT runtime property
stubs dropped from ~760k to 42. Baseline JIT now handles unsigned right shift in
the RNG callback body for the positive finite double path, cutting a measured
fill-only run to ~109ms and a full `sort.js` run to ~284ms. The remaining hot
cost is still the ~760k direct JS callback/comparator calls.

Gates:

- `array-ops.js` and `sort.js` output parity with `OTTER_JIT=0/1`.
- `OTTER_GC_STRESS=128 OTTER_JIT=1` on array callback and sort workloads.
- Targeted Test262 for `Array.prototype.{map,filter,forEach,reduce,sort,toSorted}`
  with JIT on/off failing-set parity.

### 2. Object and method residual fast paths

Target: `prop-access.js`.

Goal: close the remaining post-constructor gap now that simple constructor
field initialization no longer dominates runtime property stubs.

Work:

- Re-profile `Point.prototype.bump`, `dist2`, `p.x`, `p.y`, `p.tag`, `pts[i]`,
  and object allocation/class construct entry; every remaining fallback or
  runtime call in the hot region must be explained.
- Extend opt lowering only where a measured fallback remains:
  - dense array element load for object arrays if `pts[i]` is still a stub;
  - property store/load in inlined method bodies if any site misses the inline IC;
  - method identity guards only where the receiver/prototype chain feedback is
    monomorphic and stable.
- Keep the simple-constructor path conservative: no duplicate fields,
  `__proto__`, inherited accessors/data, proxies, derived constructors,
  `arguments`, rest params, direct eval, or observable non-store bytecode.
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

## Mandatory Post-Tier1 Work

- Cranelift backend for portable Tier2/DeepDive codegen.
- Multi-architecture JIT support for aarch64 and x86_64, with Windows x86_64
  tracked as a release-hardening target.
- CI/release gates that run the same correctness and regression checks per
  supported architecture.
- Keep arm64 dynasm only as the proven Tier1 backend until the portable backend
  is ready to replace or sit above it.
- Compare architecture coverage and release gating against Node/V8 before
  declaring multi-arch work complete.
