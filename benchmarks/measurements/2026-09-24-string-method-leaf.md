# Generated leaf hits for guarded String and collection method calls

Investigation and closing validation: September 24, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`df21bd23ab318cf5712b51087a01865196ecb6ab`. Development observations are not a
published engine baseline. The unmodified `benchmarks/scripts/native-boundary.js`
has SHA-256 `60bb4c98c42472fc2db9473646f6b54c9990479fc1870a5ded93e5706b54faf4`
and returns exactly `27000000` on every invocation.

## Where ProductionTiered lost to Template

The baseline eight-warmup/twenty-sample pilot measured Template at
132.554 ms and ProductionTiered at 143.255 ms: Production was 8.1% slower
while executing 2.7% more process instructions. Per warmed invocation the
tiers differ in exactly one repeated boundary:

| Operation per 200,000-iteration invocation | Template | ProductionTiered |
| --- | --- | --- |
| `word.indexOf("e")` (byte PC 320, `CallMethodValue`) | guarded `string_index_of_leaf` | 200,000 `jit_call_method_value` crossings |
| `word.charCodeAt(…)`, Map `get`/`set` | 600,000 explicit calls | 600,000 explicit calls |
| `Math.abs` / `Math.max` | 400,000 explicit calls | generated Int32 hits |

Totals over 28 invocations were 27,999,960 JIT-to-Rust transitions for
Template and 22,399,927 for Production (1,000,000 and 800,000 per invocation);
the Template leaf is not a counted transition. Neither tier deoptimized,
allocated on the hot path or collected.

The Production artifact shows the full cost at byte PC 320: four tagged roots
spilled, a 32-byte root record linked, logical PC 29 published, a two-word
argument packet built, `jit_call_method_value` (`reentrantValueSpan`) called,
its status decoded, the record unlinked and the roots reloaded. A five-second
native `sample` profile attributes 640 of 1,855 main-thread samples (34%) to
the `jit_call_method_value_stub` subtree: generic method dispatch, callable
activation, `impl_index_of` argument processing and the search itself. Sample
subtrees overlap; this is attribution, not an exclusive time share.

## Why Machine had no hit

The Machine IR collapse (`ae2b5c05`) removed the legacy optimizing string
intrinsics; its method-probe CFG admitted only shaped receivers with Int32
`Math` hits. The VM already snapshots `indexOf` as an exotic
`JitGuardedMethodCall` (primitive string tag, pinned `%String.prototype%`).
Two facts blocked a direct reuse:

1. `%String.prototype%` is a String wrapper. `supports_fast_property_ic`
   excludes every object with `[[StringData]]`, so it never adopts a hidden
   class and the snapshot recorded `holder_shape: 0`.
2. Template compared that `0` with the live null shape, which always matches,
   then read a fixed dictionary slot. Dictionary deletion shifts later slots
   (`remove_slot`). The saved reproducer
   `string-proto-slot-shift.js` assigns `String.prototype.lastIndexOf =
   String.prototype.indexOf` and deletes `indexOf`; the signed baseline returns
   `1` under `--jitless` instead of the interpreter's `TypeError`. This is a
   pre-existing Template miscompile.

## Selected implementation

One holder contract replaces `holder_shape: u32`: `JitMethodHolder` is
`Receiver`, `Shape(u32)` or `Dictionary(u64)`. The dictionary form reuses the
existing isolate-wide `dictionary_shape_id`, already guarded by global
bindings, which every add, delete, `delete_if_same_data` and descriptor change
replaces. An unchanged id proves each key still owns its captured slot; the
existing builtin identity guard then proves that slot's live value.

- Template's guarded method call checks the same layout (null shape plus id),
  so the slot-shift reproducer now throws `TypeError` in every tier.
- Machine proves an exotic receiver with the existing
  `CacheIrLoadIntrinsicPrototype` (now admitting primitive strings, whose only
  own keys are `length` and indices), then either the existing
  `CacheIrGuardShape` plus ordinary state (fast holders such as
  `Map.prototype`) or the new `CacheIrGuardDictionaryLayout`.
- The hit is the new `NativeLeafProbe`: receiver and at most one argument in
  the leaf ABI registers, the declared `LeafNoAllocStub2` entry called
  directly, its status bit becoming the hit. It publishes no root record,
  safepoint, VM PC or argument packet and decodes no runtime status; it
  cannot allocate, collect, throw or reenter. A proof or leaf miss enters the
  existing committed method call, which performs the whole operation once.
  No deopt or replay follows an effect. No string pointer is cached in code
  and no raw view outlives the call.
- Both AArch64 and x86-64 Machine lower the two operations. The x86 static
  leaf helper takes the entry id and word count, shared by static calls and
  probes. No second ABI, dual dispatch or compatibility path remains.

The same admission covers any guarded `CallMethodValue` whose declared entry is
a pure leaf: String `charCodeAt`/`codePointAt`/`indexOf`/`includes`/
`startsWith`/`endsWith`, Map `get`/`has` and Set `has`. Mutating and
allocating entries keep the committed call. Interrupt semantics match the
general native, which also searches without an interrupt check.

The final artifact (`art-final`) shows the byte PC 320 hit as
`machineCacheIrLoadIntrinsicPrototype` → `machineCacheIrGuardDictionaryLayout`
→ `machineCacheIrLoadField` → `machineNativeLeafIdentity` →
`machineNativeLeafProbe`: `ldr x0, [x19]; ldr x0, [x0, #0x28]; blr
string_index_of_leaf; cbnz x1, miss`. The cold `machineGenericMethodCall`
sibling owns the one remaining safepoint; the function still has 19 static
safepoint records. Code grows from 10,064 to 10,540 bytes.

## Corrections found by the new stress matrix

Both are pre-existing moving-GC errors in shared interpreter/native paths,
fixed at their source and covered by regressions that fail on the old code:

- Map/Set/WeakMap/WeakSet/generator `[[Set]]` allocated the lazy expando bag
  and then reused unrooted `target`, `value` and `receiver`
  (`shadowed.get = …` under `OTTER_GC_STRESS=16`). They now travel through the
  existing handle scope and are re-read after the allocation.
- `indexOf`/`lastIndexOf` flattened a rope receiver while the search string
  was an unrooted handle (`OTTER_GC_STRESS=4`). One helper renders and
  flattens both operands in rooted slots; two already-contiguous strings
  return immediately because nothing can allocate.

## Final paired result

Three directly alternating pairs per tier, eight warmups and twenty samples,
pair 2 reversed. The after binary is byte-for-byte the executable built by the
successful closing gate (`add8bc03…`). No builds, tests or agents ran during
the series. All 18 processes validate checksum `27000000`.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 342.385 → 339.429 | 342.870 → 338.117 | 343.537 → 338.787 | **−1.21%** |
| Template | 132.223 → 132.062 | 131.852 → 132.148 | 132.894 → 132.389 | **−0.09%** |
| ProductionTiered | 145.567 → 97.262 | 143.366 → 97.566 | 143.584 → 97.359 | **−32.44%** |

| Paired process counters | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Retired instructions | −0.33% | +0.02% | −29.16% |
| Hardware cycles | −1.23% | −0.13% | −32.26% |

ProductionTiered is now 26.3% faster than Template instead of 8.1% slower.
Template is unchanged within noise: its hit already used the leaf, and its
guard differs only by the layout-id compare (7,972 → 7,980 code bytes). The
small Interpreter gain comes from the contiguous search-operand early return.

| Totals across 28 invocations | Template before → after | ProductionTiered before → after |
| --- | ---: | ---: |
| JIT-to-Rust transitions | 27,999,960 → 27,999,960 | 22,399,927 → 16,799,952 |
| Runtime stub transitions | 33,601,346 → 33,601,346 | 28,001,269 → 22,401,294 |
| Property stubs | 5,599,992 → 5,599,992 | 5,599,975 → 5,599,975 |
| Optimizing / generated-call deopts | 0 / 0 → 0 / 0 | 0 / 0 → 0 / 0 |
| Code generations / optimized entries | 1 / 0 → 1 / 0 | 1 / 28 → 1 / 28 |
| Emitted code bytes | 7,972 → 7,980 | 10,064 → 10,540 |
| Managed cells / bytes | 384 / 145,904 → same | 486 / 160,184 → same |
| Full / minor collections | 0 / 0 → 0 / 0 | 0 / 0 → 0 / 0 |

Production removes 5,599,975 crossings: 199,999 per invocation averaged over
28 (initial OSR accounts for the fraction), exactly 200,000 in a warmed
invocation per the runtime regression. Stub transitions fall from 1,000,045
to 800,046 per invocation. Remaining crossings are `charCodeAt` and Map
`get`/`set` (explicit `LoadProperty` + `CallWithThis`) plus one `new Map()`.

Production JIT compile time, median of the three runs, rose from 3.72 ms to
7.46 ms (range 6.10–8.90 ms), once during warmup; the pilot measured 3.59 →
4.09 ms. It is outside the timed samples and does not explain the steady gain.
Interpreter retains exactly 33,600,311 managed allocations, 4,704,137,952 bytes
and 192 full/minor collections; pause totals overlap between sides.

At **732 gross hand-written production LOC added** (165 removed), the
ProductionTiered wall gain is **0.04432 percentage points per added line**;
Interpreter 0.00165, Template 0.00012. The count, in `loc-audit.txt`, includes
comments and blank lines in production Rust files and excludes tests, inline
test modules, benchmarks and documentation.

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source (`gate-closed.log`): fmt, warnings-denied all-targets
  Clippy, VM/JIT/bytecode tests, release verifier and adversarial corpus,
  42/42 differential cases, and the kernel ledger (Template 131.94 ms,
  1,200,048 stubs; Production 97.32 ms, 800,046 stubs, 199,999 property stubs,
  zero deopts).
- AArch64 focused runtime matrix, eight test binaries: 42/42 at each of
  `OTTER_GC_STRESS` unset, 16, 4 and 1. Rosetta x86-64: 39/39 at each level
  (AArch64-only artifact assertions excluded by `cfg`).
- VM string unit tests 113/113 at each stress level.
- Target-neutral Machine suite: 173/173 AArch64, 168/168 x86-64 (Rosetta),
  including the new `NativeLeafProbe` verifier contract test.
- Targeted Test262 on AArch64 and Rosetta, corpus `be13516f`: Map, Set,
  Object, String and `language/expressions/call` exactly match the HEAD
  engine's saved totals and failing sets on each target: 5,427 total, 5,420
  pass, 7 skip, zero failures, crashes, timeouts or OOMs.
- `git diff --check` passed.

New regressions in `jit_machine_string_method_leaves.rs` cover warm leaf hits
with zero JIT-to-Rust transitions, prototype additions and deletions,
same-slot replacement, getter installation, String wrapper and rope
receivers, UTF-16 receivers, Map prototype replacement and instance shadows,
the dictionary slot-shift `TypeError`, moving collection between hits, and the
two rooting corrections. `optimizing_string_intrinsics` now asserts the
leaf relocation and layout-proof regions instead of the removed legacy
inline intrinsic; the native-boundary kernel test pins 600,001 transitions per
warmed Production invocation.

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

Raw records and [environment](2026-09-24-string-method-leaf-environment.json),
[run counters](2026-09-24-string-method-leaf-runs.csv) and
[wall samples](2026-09-24-string-method-leaf-samples.csv) are tracked here.
Binaries, logs, JIT events, artifacts, native profiles and the reproducer are
retained under ignored `benchmarks/results/indexof-leaf-2026-09-24/`.
