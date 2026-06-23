# Otter Engine Refactor Progress

**Updated:** 2026-06-22
**Plan source:** [`plan.md`](plan.md)  
**Baseline before current slice:** `7a4506d8 perf(jit): fast-path unsigned shifts`

This log keeps only live Tier1 work. Completed history was removed from this
file on purpose; use `git log` for landed slices.

## Active Snapshot

Fresh release binary: `target/release/otter`.

| script | JIT off | JIT on | Node | status |
|---|---:|---:|---:|---|
| `mandelbrot.js` | 0.89s | 0.03s | 0.03s | closed |
| `nbody.js` | 0.89s | 0.07s | 0.03s | closed enough for Tier1 |
| `fib.js` | 1.27s | 0.09s | 0.04s | closed enough for Tier1 |
| `typed-array.js` | 1.97s | 0.09s | 0.03s | closed enough for Tier1 |
| `prop-access.js` | 2.11s | 0.17s | 0.03s | open |
| `array-ops.js` | 2.36s | 0.45s | 0.09s | open |
| `sort.js` | 2.76s | 0.25–0.53s | 0.16s | open; fill-loop direct calls closed; sort runtime still open |
| `json.js` | 0.95s | 0.93s | 0.39s | open; serializer + small-dict allocation closed, parse object shaping remains |
| `string-ops.js` | 0.43s | 0.44s | 0.03s | open |
| `regex.js` | 2.21s | 2.20s | 0.03s | open |

Output parity across `benchmarks/scripts/*.js` passed after the latest JIT
commits. GC-stress smoke passed for `fib`, `prop-access`, and `nbody`.
Simple constructor field initialization now bypasses the hot `this.x/y/tag`
store-property stub path for `prop-access.js`; measured JIT runtime property
stubs dropped to 4 on the full benchmark and 0 on construct-only isolation.
`sort.js` now bypasses the hot dense-array fill-loop `StoreElement` bridge after
bounded `new Array(n)` dense-hole construction and guarded dense-array store
lowering. Full benchmark JIT runtime property stubs dropped from ~760k to 42.
The RNG callback body's unsigned right shift no longer delegates from baseline
JIT for positive finite double inputs. Tiny closure-upvalue leaf calls inline
through a guarded closure-validation helper, so the RNG fill loop no longer
publishes a callee frame or enters compiled direct-call code on every iteration.
Measured fill-only is ~73ms, prefilled sort is ~147ms, and full `sort.js` is
~254ms; `jitDirectCalls` dropped from ~760k to 0. The remaining cost is the
per-call closure validation helper plus native sort/runtime work.

## Architecture Scope

The active measurements are arm64-focused because the current Tier1 backend is
hand-emitted arm64 dynasm. Multi-architecture support is still mandatory:

- Cranelift is the planned post-Tier1 backend for portable Tier2/DeepDive
  codegen.
- Required targets remain aarch64 and x86_64; Windows x86_64 is tracked under
  release hardening once the portable backend is integrated.
- Future progress entries must state whether a JIT slice is arm64-only,
  Cranelift-portable, or runtime-only.
- Node/V8 is the reference point for architecture coverage, tiering behavior,
  deopt/safepoint practice, and release gates. Do not copy names; do compare
  mechanics and verification standards.

## Remaining Queue

### Open 1: callback-heavy builtins

Benchmarks: `array-ops.js`, `sort.js`.

Next actions:

- Add or use focused counters for array callback and sort comparator paths.
- Measure the residual cost inside the RNG closure-validation helper and native
  sort runtime now that dense fill-loop stores and direct-call frame publishing
  no longer dominate `sort.js`.
- Cut the measured cost without changing generic Array semantics.
- Re-run parity, GC stress, and targeted Array Test262 subsets.

Risk:

- Callback side effects can mutate the receiver, prototype, species constructor,
  output arrays, and global state. Any fast path must have a precise fallback.

### Open 2: object/method residual overhead

Benchmark: `prop-access.js`.

Next actions:

- Re-profile after simple-constructor fast initialization.
- Explain remaining allocation/class-entry, `pts[i]`, method-call, and
  property-load/store costs.
- Patch only measured misses in dense element load, property load/store,
  object allocation, or method identity/inlining.
- Keep exact deopt frame state and write barriers intact.

Risk:

- Store fast paths must never skip the GC barrier.
- Method/property guards must account for prototype and shape changes.

### Open 3: string builtins

Benchmark: `string-ops.js`.

Next actions:

- Split the benchmark into string build, split/join, char scan, slice/indexOf.
- Implement contained fast paths for the dominant operation first.
- Gate with String Test262 subsets and GC stress.

Risk:

- Unicode, ropes/substrings, and coercion order make broad shortcuts unsafe.

### Open 4: JSON runtime throughput

Benchmark: `json.js` (5.1x -> 3.4x vs Node this slice; on == off, pure runtime).

Done:

- Stringify hot path is now allocation-free (per-shape key-list cache, Latin-1
  / `&str` in-place quoting, number-into-buffer rendering). Micro stringify
  773ms -> 294ms, output byte-identical, JSON Test262 unchanged.
- Small dictionary objects no longer keep a per-object hash index (linear scan
  under `DICT_LINEAR_SCAN_MAX`), cutting the parse `contains_key` hot spot.
  Micro parse 681ms -> 608ms.

Next lever (the remaining ~4x on parse):

- `JSON.parse` builds dictionary-mode objects (`empty_dictionary_object_body`
  + per-key `object::set`), so every parsed record allocates an `ExoticSlots`
  box + `dictionary_keys` Vec + values Vec and pays per-key dictionary work.
  Build **shaped** objects instead (one shared hidden class per record shape,
  bulk slot init like the simple-constructor path). This needs `ShapeRuntime`
  threaded into the parser (or a rooted dictionary->shape normalization in the
  existing post-parse prototype walk). HIGH GC RISK: shape transitions allocate
  mid-build, and JSON paths have a history of use-after-move bugs — gate on
  full GC stress + JSON Test262 differential, do not rush.

Risk:

- JSON correctness failures are easy to hide behind benchmark-shaped records.
  Conformance must gate every parser/stringifier change.

### Open 5: regex runtime throughput

Benchmark: `regex.js` (27x vs Node; on == off, pure engine).

Profile: ~78% in the backtracking engine itself — `Matcher::run` 41%,
`Matches::next` 26%, `class_member` 11% (already ASCII-bitmap fast-pathed).
The gap is architectural: a backtracking interpreter vs Node's Irregexp
(regex compiled to native code). The benchmark patterns (`\b[a-z]{4,}\b`,
`\d+`, `([a-z.]+)@([a-z.]+)`) are all backreference-free / regular, so the
real lever is a Thompson NFA / lazy-DFA (Pike VM) execution path for
backref-free programs — O(n) instead of backtracking — falling back to the
current backtracker only for patterns that need it. This is the RippleRegex
workstream, not a contained tuning slice; the prior prefilter/bitmap commits
already mined the incremental wins.

Next actions:

- Measure `match`, `replace`, and `exec` separately.
- Choose a contained regex-engine improvement slice.
- Gate on RegExp Test262 subsets before treating the benchmark as closed.

Risk:

- `lastIndex`, captures, replacement expansion, Unicode, and case folding are
  observable. Benchmark-only regex shortcuts are not acceptable.

## Required Checks Per Slice

- `cargo build --release -p otter-cli`
- `cargo test -p otter-jit`
- `cargo test -p otter-vm`
- `OTTER_JIT=0/1` output parity for all `benchmarks/scripts/*.js`
- `OTTER_GC_STRESS=128 OTTER_JIT=1` for touched workloads
- Targeted Test262 subsets for touched builtins/opcodes
- Debug marker grep before commit:
  `CALLDBG`, `METHODDBG`, `OTTER_TMP_METHOD_FALLBACK`, `NODISABLE`,
  `OSRDBG`, `PREOSR`, `WBAIL`, `OSROUT`

## Current Non-Goals

- Starting broad Tier2/peak optimizer work before the remaining Tier1 gaps are
  either closed or explicitly moved to a runtime subsystem owner.
- Adding feature flags or env kill-switches.
- Landing benchmark-fit shortcuts that bypass object, callback, GC, string,
  JSON, or RegExp semantics.
- Treating arm64-only dynasm as the final architecture story.
