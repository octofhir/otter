# A boundary-free native-boundary kernel

Investigation and closing validation: September 24–25, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`f9db70f7511f02ae4a6d362dbe129b430a0952a7`. Development observations are not a
published engine baseline. The unmodified `benchmarks/scripts/native-boundary.js`
has SHA-256 `60bb4c98c42472fc2db9473646f6b54c9990479fc1870a5ded93e5706b54faf4`
and returns exactly `27000000` on every invocation.

## Starting point

After the previous slice, each warmed invocation still crossed the boundary
400,000 times on both JIT tiers. That was 200,000 `table.set` calls and
200,000 cold `word.charCodeAt` property loads. A native `sample` of both tiers
also found a self-inflicted cost: `BootstrapNative::original_native_fn`,
`prototype_bridge` and `jit_static_call_target` held about 330 Template samples
(Production shows the same functions). Every generic call re-classifies its
callee on the feedback path. The larger registry resolved each row's identity
through a name `match` before comparing it.

## Changes

1. **Classifier.** Registry rows now hold the builtin function pointer itself.
   The classifier reads the callee's static entry once and compares addresses,
   which removes the per-row name matching. The `BootstrapNative` indirection
   and the now-dead `math::original_native_fn` are deleted.
2. **In-place mutating leaves.** `runtime_stubs::leaf_entry_shape` is the one
   description of a non-allocating entry: the pure two-word family, and the
   in-place mutating two- and three-word families, which carry their own write
   barrier and miss instead of allocating. `Map.prototype.set` is declared as a
   `this_operand` leaf over `collection_map_set_mutating`, which overwrites an
   existing key and misses on insertion.
   - Machine `NativeLeafProbe` accepts up to three words. A mutating probe
     reads and writes the heap and is never commoned or hoisted.
   - The x86-64 leaf call now serves all three families.
   - Template's explicit-receiver leaf uses the same shape.
3. **String prototype loads.** A new `JitCacheIrOp::GuardDictionaryLayout`
   lets named loads on primitive strings use the intrinsic receiver proof plus
   the `%String.prototype%` dictionary-layout id. It lowers to the existing
   Machine `CacheIrGuardDictionaryLayout`, the shared Template AArch64 emitter,
   and a new Template x86-64 sequence. `length` and canonical numeric names,
   which a primitive string can own, never produce a prototype program.

## Correctness findings fixed at their source

- **Dictionary layout id.** Redefining an existing slot of a dictionary-mode
  object in place (`set_slot(.., None)`) changed its kind or attributes
  without a new `dictionary_shape_id`. Shaped objects transition to a new
  hidden class in the same case. Generated String loads therefore kept reading
  the slot after an accessor was installed over `charCodeAt`; the existing
  `builtin_prototype_accessor_misses_execute_once` test caught it. Any in-place
  change of flags or kind now assigns a fresh id. Same-slot value writes keep
  it, so they stay generated hits read live.
- **Out-of-range string index (pre-existing interpreter bug).**
  `"x"[1]` returned `undefined` without consulting `%String.prototype%`.
  Node returns the prototype's own `1` property. Per §10.4.3.5, only indices
  below the length are own properties, so lookup now continues to the
  prototype.
- **Rope flattening root (pre-existing moving-GC bug).** When concatenation
  exceeds the rope depth budget, `concat_string_bodies` flattened the deeper
  side while the other side sat in an unrooted local copy. Found by
  `string_rope_slice` at `OTTER_GC_STRESS=16`. The other side now stays in a
  rooted slot across the flatten.

## Final paired result

Three directly alternating pairs per tier, eight warmups and twenty samples,
pair 2 reversed. The before binary is the previous slice's gate build
(`29b0cbd9…`). The after binary is this slice's closing-gate build
(`05da46b9…`). All 18 processes validate checksum `27000000`. External load
averaged about 5 during the series; no builds, tests or agents ran.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 340.896 → 340.714 | 341.167 → 362.533 | 339.034 → 338.801 | +2.00% (outlier; see control) |
| Template | 61.502 → 21.768 | 61.194 → 21.690 | 61.284 → 21.815 | **−64.52%** |
| ProductionTiered | 63.840 → 22.997 | 63.916 → 22.855 | 64.098 → 22.795 | **−64.22%** |

| Paired process counters | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Retired instructions | −0.00% | −63.21% | −63.20% |
| Hardware cycles | +2.26% | −63.78% | −63.45% |

The Interpreter's +2.00% comes from one pair (362.533 ms) with an unchanged
instruction count. An immediately repeated Interpreter-only three-pair control
measured 339.370 → 340.223, 339.148 → 339.350 and 341.479 → 341.610 ms:
**+0.12% wall, +0.002% instructions, −0.006% cycles**. No Interpreter gain or
regression is claimed. Its allocation and collection totals are identical.

| Totals across 28 invocations | Template before → after | ProductionTiered before → after |
| --- | ---: | ---: |
| JIT-to-Rust transitions | 5,599,992 → 1,784 | 5,600,002 → 1,794 |
| Runtime stub transitions | 11,201,378 → 3,178 | 11,201,344 → 3,161 |
| Property stubs | 5,599,992 → 0 | 5,599,975 → 0 |
| Optimizing / generated-call deopts | 0 / 0 → 0 / 0 | 0 / 0 → 0 / 0 |
| Emitted code bytes | 8,348 → 8,564 | 10,908 → 11,416 |
| Managed cells / bytes | 384 / 145,904 → same | 486 / 160,184 → same |

Per warmed invocation both tiers go from about 400,000 crossings to 65. The
remaining crossings are the 64 inserting `set` calls into each fresh Map and
the `new Map()` construct; the gate ledger reports 114 / 113 stubs per
invocation. Production compile-time medians are 7.03 → 7.21 ms, outside the
timed samples. Both tiers now run the kernel in about 22 ms, down from
63–64 ms. The two tiers are within 5% of each other: Template 21.8 ms,
Production 22.9 ms.

At **435 gross hand-written production LOC added** (273 removed), the wall
gain per added line is **0.1476 percentage points for ProductionTiered** and
0.1483 for Template (`loc-audit.txt`).

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger
  (Template 21.67 ms / 114 stubs, Production 22.81 ms / 113 stubs, zero
  deopts).
- Focused runtime matrix across 16 test binaries: 63/63 on AArch64 and 58/58
  under Rosetta x86-64 at each of `OTTER_GC_STRESS` unset, 16, 4 and 1. VM
  string unit tests passed 113/113 at each level.
- Machine suites 173/173 (AArch64) and 168/168 (x86-64); VM registry and
  dictionary-id unit tests passed.
- Targeted Test262 on AArch64 and Rosetta, with zero failures, crashes,
  timeouts or OOMs in any section:
  - Map, Set, Object, String, `language/expressions/call` and Math exactly
    match the previous slice's totals and failing sets.
  - `language/expressions/property-accessors` 21/21, `built-ins/Reflect`
    154/154 and `language/types/string` 24/24. None of these sections
    appears in the ES_CONFORMANCE failing set.
- `git diff --check` passed.

New regressions:
- `jit_string_prototype_loads` covers warm loads with zero property stubs;
  same-slot replacement read live; added and deleted keys; getter
  installation and removal; own `length` and index keys against prototype
  definitions; the deleted-and-shifted slot; and moving collection.
- `jit_resolved_native_leaves` gains `map.set` overwrites with zero crossings,
  insertion misses, a replaced `set`, a non-Map receiver, and young values
  stored into a promoted Map under stress.
- VM unit tests pin the dictionary-id contract and the registry shapes.

## Reproduction

Build `otter-engine-benchmark` with `cargo build --locked --release
-p otter-benchmark --features engine --bin otter-engine-benchmark`, keep
separate before/after executables, and run each tier serially with
`OTTER_GC_STRESS` unset:

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/native-boundary.js \
  --function engineKernel --expected 27000000 \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-boundary-free-kernel-environment.json),
[run counters](2026-09-25-boundary-free-kernel-runs.csv) and
[wall samples](2026-09-25-boundary-free-kernel-samples.csv) are tracked here.
Binaries, logs, profiles, the Interpreter control and Test262 records are
retained under ignored `benchmarks/results/next-2026-09-25/`.
