# Native-boundary attribution and generated collection property loads

Investigation and closing validation: September 21–22, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`398609e8bb32e2992f00b7525c741ddab0059e6b`. Development observations are not a
published engine baseline. The unmodified `benchmarks/scripts/native-boundary.js`
has SHA-256 `60bb4c98c42472fc2db9473646f6b54c9990479fc1870a5ded93e5706b54faf4`
and returns exactly `27000000` on every invocation.

## Why ProductionTiered was slower

The initial clean release run used eight warmups and twenty measured samples:
Interpreter 393.478084 ms, Template 187.261917 ms, ProductionTiered 225.568063 ms.
Production was 20.46% slower than Template despite fewer counted runtime stubs.
Process retired instructions were 145,517,818,649 / 73,875,216,433 /
86,084,731,167 respectively; Production executed 16.53% more than Template.
No tier deoptimized. Compilation was 0.515 ms total for Template and 3.780 ms
for Production across the entire run, outside the steady measured samples.

The bytecode and generated artifact account for the hot crossings:

| Operation per 200,000-iteration invocation | Template | ProductionTiered |
| --- | --- | --- |
| `word.length`, returned string `.length` | Generated | Generated |
| `charCodeAt` named load | 200,000 cold property loads | 200,000 cold property loads |
| `Map.set` and `Map.get` named loads | 400,000 cold property loads | 400,000 cold property loads |
| `charCodeAt` and Map calls | 600,000 general explicit calls | 600,000 general explicit calls |
| `Math.abs` and `Math.max` | 400,000 general explicit calls | Generated Int32 hits |
| `indexOf("e")` | Guarded native leaf | 200,000 general method calls |

Thus Template has approximately 1,600,048 counted stubs and Production
1,400,044. These are event counts, not time shares. The Template native leaf
is not included in the generic runtime-stub counter. Its absence must not be
reported as a generated string-search body or zero native calls.

At byte PC 320, Production `indexOf` has a safepoint with four tagged roots
(word array, Map, word, needle). Assembly spills these roots, links a 32-byte
root record, publishes logical PC 29, constructs the receiver/argument packet,
calls the general method stub, then unlinks and reloads the roots. The parent
NativeFrame already exists; this is not a new complete NativeFrame per call.
The cold property calls also resolve the code-owned safepoint metadata even
when no inline descendants need reconstruction. No executed safepoint count
is inferred from the artifact's nineteen static records.

Separate five-second native sampling runs identify general method dispatch,
property dispatch, argument/root handling, safepoint lookup and native String
processing. In the Production profile, the generic method subtree contains
1,466 of 4,170 samples. `indexOf` flattens each already-flat operand through
`JsString::flatten_in_place`, which constructs a Rust RootScope before its
body-level no-op check: 400,000 avoidable scope constructions per invocation.
Sample subtrees overlap and do not provide additive exclusive time shares.
The profiled runs are not used as performance samples.

## Allocation and GC attribution

Kernel diagnostics now snapshot existing GC telemetry outside timed samples,
covering warmups plus measurements like the existing JIT counters. A detached
checkout of the signed baseline was rebuilt with only this benchmark telemetry
patch, retaining the original engine implementation. Its eight-plus-twenty run:

| Metric, total across 28 invocations | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Wall median, ms | 390.092730 | 175.580146 | 217.937292 |
| Managed allocated cells | 33,600,311 | 359 | 461 |
| Managed allocated bytes | 4,704,137,952 | 144,672 | 158,952 |
| Full / minor collections | 192 / 192 | 0 / 0 | 0 / 0 |
| Full-GC pause total, ms | 1,023.766329 | 0 | 0 |
| Minor-GC pause total, ms | 216.971916 | 0 | 0 |

Full-GC pauses include their minor collection; these times must not be summed.
The tier gap is not a GC pause effect. Managed allocation counters exclude Rust
allocator traffic such as RootScope construction and external backing stores.
The previous `cost.sh` zero for GC was inconclusive because kernel records had
not emitted GC metrics at all.

## Selected implementation

Map and Set named loads compose a new `LoadIntrinsicPrototype` CacheIR receiver
proof with the existing shape, atom-slot and live field operations. The proof
uses the exact cell type and existing no-expando/no-prototype-override latch,
then selects the pinned realm prototype. One shared receiver DTO is also used
by the existing native method snapshots. There is no new cache or runtime ABI.
Template and Machine emit the same proof on AArch64 and x86-64.

The final snapshot audit also found a Template interaction: preparing a
collection prototype after its method guard was captured could replace a
bootstrap dictionary shape and make an existing guarded native call miss.
Property preparation now precedes nested snapshots, and collection method
snapshots independently prepare their prototype and resolve the current own
name, builtin identity and slot. This also protects method-only code compiled
before a separate named-load function. Immutable proofs never freeze the old
dictionary identity. Dedicated mixed-body and cross-function regressions cover
the existing native hit alongside the new property hit.

A saved reproduction on the AArch64 Template tier in a fresh Interpreter
confirms the intermediate regression: the original engine performed zero general native calls and 399,986 property
calls across one warmup and one sample; the first property candidate instead
performed 399,986 general native calls and zero property calls. Both returned
`1400000`. The source is `ordering-bare-mixed.js`, SHA-256
`a2da8bba00a9d091b1789f81a88bffb9a973bdd748aecfdb8b7d7616650beb09`.
Runtime bootstrap can prepare this prototype earlier, so the regression must
also run through the benchmark's fresh Interpreter.
The final release benchmark restores both counters to zero on Template in
that same mixed-body reproduction, without optimized or generated-call deopts.

The extra cross-function diagnostic also exposed an existing x86 optimizing
error: accumulating calls to an already compiled method inside a 20,000-step
loop returns `301` instead of `140000`. The original signed engine reproduces
the exact result under Rosetta; Interpreter and Template pass, and AArch64
passes all three tiers. Its source and original/candidate results are retained
as `ordering-bare-cross-function.js` and `validation-x86-cross-function-baseline.log`.
This is separate from the unmodified native-boundary workload, which passes.
The installed-guard regression therefore performs the same 20,000 property
loads and calls the already compiled method after the loop, with all three
tier checksum and zero-deopt assertions retained. A setup invocation with zero
property iterations primes that call site before OSR. Original AArch64 Template
then has zero generic calls; the broken property candidate has two avoidable
native misses across the two invocations. This tests the preparation contract
without treating the unrelated baseline optimizing failure as a passing test.
The final release benchmark passes all three tiers with no deopts; Template
has zero property or generic call transitions, and Production retains its two
existing general native calls while eliminating the property transitions.

Snapshot preparation migrates relevant bootstrap prototype data holders through
the existing rooted slow-to-fast operation and rejects a null shape. An initial
candidate omitted this step: isolated Runtime tests had already prepared the
prototype, whereas the benchmark's fresh Interpreter still held a null shape.
Its invalid program rejected Machine compilation and missed in Template. The
full original kernel exposed the failure before acceptance; dedicated benchmark
and Runtime regressions now require optimizing admission and the reduced
property-transition count without prototype preconditioning.

Same-slot data replacement is read live. Accessors, instance shadows, custom
prototypes, incompatible receiver tags and failed slot proofs enter the one
committed canonical load. A generated hit has no allocation, reentry, throw,
frame publication or safepoint. Loaded callables remain ordinary tagged values
across later argument evaluation. Collection `size` keeps its existing
specialized path. String prototype loads are outside this slice:
their StringData exotic state cannot use the ordinary-object proof unchanged.

A separate baseline control exposed an existing `size` discrepancy: replacing
the Map/Set prototype accessors with data values after warmup yields those
values (`900`/`901`) in Interpreter but the collection count (`1`/`1`) in both
JIT modes. The saved signed-baseline binary reproduces it. This slice does not
change the specialized `size` operation and excludes that key from the new
programs; the reproducer remains in local evidence. No pre-existing assertion
or workload was relaxed to accommodate it.

The new getter stress matrix also exposed a baseline moving-GC error in the
interpreted primitive property driver. Boxing could move a String receiver
before the driver passed its stale copied value to lookup/getter invocation.
The driver now reloads its existing traced source register after boxing and
lookup. The signed-baseline CLI fails the minimal stress-1 reproducer; the
getter assertion remains enabled in all tiers and both native targets.

## Property candidate comparison (before ordering correction)

This intermediate series consists of three directly alternating before/after pairs
per tier, each with eight warmups and twenty samples. Pair 2 reverses the order.
The baseline engine is unchanged apart from the same untimed GC diagnostics.
All checksums match. No agent builds or tests overlapped these measurements.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 449.975 → 466.028 | 366.993 → 380.775 | 415.815 → 386.999 | +0.003% |
| Template | 176.410 → 140.782 | 173.731 → 136.617 | 172.915 → 139.777 | **−20.25%** |
| ProductionTiered | 240.877 → 192.876 | 217.074 → 172.985 | 217.739 → 188.290 | **−17.98%** |

Paired change is the geometric mean of the three after/before median ratios.
This retains the matched runs instead of dividing unrelated medians under
varying background load. The speedup is 1.254× for Template and 1.219× for
Production. Production remains slower than Template; the general native-call
cost identified above remains a subsequent bottleneck.

| Metric | Template before → after | ProductionTiered before → after |
| --- | ---: | ---: |
| Property stubs / invocation | 599,999.143 → 199,999.714 | 599,997.321 → 199,999.107 |
| Total runtime stubs / invocation | 1,600,047.500 → 1,200,048.071 | 1,400,043.536 → 1,000,045.321 |
| General native-call transitions / invocation | 999,998.571 → 999,998.571 | 799,997.393 → 799,997.393 |
| Retired instructions, paired change | −19.35% | −18.74% |
| Hardware cycles, paired change | −19.86% | −18.56% |
| JIT compiler total, median of runs | 0.093375 → 0.106208 ms | 2.796375 → 3.152000 ms |
| Emitted code bytes | 7,668 → 7,972 | 9,168 → 10,064 |
| Code generations / optimizing deopts | 1 / 0 → 1 / 0 | 1 / 0 → 1 / 0 |
| Managed allocated cells, all 28 calls | 359 → 384 | 461 → 486 |
| Managed allocated bytes, all 28 calls | 144,672 → 145,904 | 158,952 → 160,184 |
| Full / minor collections | 0 / 0 → 0 / 0 | 0 / 0 → 0 / 0 |

The 25 additional cells / 1,232 bytes are prototype shape preparation during
compilation, not an allocation on a generated hit. The repeated operation
removes 400,000 cold property transitions per invocation. It does not remove
the Map native calls themselves. Initial OSR accounts for the small fractional
per-invocation averages above.

Interpreter retains exactly 33,600,311 managed allocations, 4,704,137,952 bytes
and 192 full/minor collections across each 28-call run. Its paired instructions
increase 0.65% and cycles 1.52%, while paired wall time is effectively unchanged.
This slice makes no Interpreter speedup claim. Wall uncertainty is material:
an additional three-pair Interpreter control produced before/after medians of
490.096/514.645, 1002.885/505.304 and 534.477/513.785 ms under other user workloads.
That inconsistent control cannot establish a small wall-time difference.

A preceding post-clean series is retained but excluded from the accepted score:
multiple filesystem indexers were processing an external full Cargo cleanup,
and even Interpreter moved from 389 to 789 ms. No user processes were stopped.
The stable JIT instruction and crossing reductions support the wall results;
the measurements remain local observations, not an overall-engine claim.

The complete run summaries and individual wall samples, including noisy
controls and intermediate candidates, are checked in as [run counters](2026-09-21-native-boundary-runs.csv) and
[wall samples](2026-09-21-native-boundary-samples.csv).
[Environment and binary hashes](2026-09-21-native-boundary-environment.json)
record the source hash, exact workload, compiler, command and aggregation.

These measurements precede the snapshot-ordering correction and are retained
as intermediate evidence, not the final commit score.

## Closing Interpreter check and contiguous-string correction

After the snapshot-ordering correction, three quiet directly alternating pairs
found a small but repeatable Interpreter cost: 384.402 / 383.956 / 388.541 ms
before and 392.018 / 393.173 / 392.900 ms after. Paired wall time increased
1.83%, hardware cycles 2.10%, and instructions 0.38%. Template improved 19.80%
and Production 20.78%, but that candidate was not accepted. Every Interpreter
run retained identical managed allocations, root updates and collection counts.
Six further isolation runs retained the same tendency for both the candidate
before ordering repair and the repaired candidate.

The primitive receiver correction remains necessary: the original engine fails
its moving-GC reproducer. An assembly audit shows the reload is inlined, with
about five extra instructions on that path and no additional stack frame.
The already-profiled native String overhead offers a direct way to remove work
without discarding the root fix. `JsString::flatten_in_place` now checks the
existing allocation-free contiguous-view predicate before constructing a
RootScope. Its underlying implementation already used this identical predicate
as a no-op check. Real materialization, roots, receiver identity and barriers
are unchanged; no second string representation or native-call contract is added.

The general `String.prototype.indexOf` path flattens both operands. For this
kernel's already-contiguous inputs, the change avoids 400,000 RootScope
constructions per invocation in Interpreter and Production. Template's guarded
`string_index_of_leaf` already searches directly without flattening, so this
correction does not remove another Template crossing. It explains why the
incremental benefit is concentrated in Interpreter and Production. Runtime
crossing counts are unchanged by this seven-line addition; the Map property
proof remains responsible for eliminating 400,000 property calls.

A fresh eight-warmup/twenty-sample pilot validated all checksums: Interpreter
400.150 → 373.709 ms, Template 178.813 → 142.278 ms, Production
223.038 → 150.988 ms. This pilot confirmed the correction before restarting
the closing gate. The final paired series below supplies the commit score.

## Final paired result

All validation was repeated after the contiguous-string correction. The final
release benchmark is byte-for-byte the executable built by the successful full
gate. Three directly alternating pairs per tier use the same unchanged source,
eight warmups and twenty samples; pair 2 reverses order. No agent builds or
tests ran during this series. All 18 processes validate checksum `27000000`.
Only the `closed` series supplies the following commit score.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: |
| Interpreter | 394.049 → 367.027 | 393.344 → 365.047 | 395.048 → 372.951 | **−6.55%** | 1.070× |
| Template | 180.293 → 141.682 | 179.611 → 141.721 | 179.190 → 141.703 | **−21.14%** | 1.268× |
| ProductionTiered | 222.630 → 151.500 | 221.656 → 150.951 | 222.644 → 151.693 | **−31.91%** | 1.469× |

| Paired process counters | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Retired instructions | −8.45% | −20.43% | −31.08% |
| Hardware cycles | −6.28% | −20.91% | −32.72% |

Production remains about 6.8% slower than Template in this final series,
compared with about 23.7% in its matched baseline. Its general `indexOf`
boundary and frame/root publication remain; removing redundant work inside
that call narrows the gap without changing its transition count.

The final property, total-stub and general native-call counts equal the
intermediate counter table above: approximately 600,000 → 200,000 property
stubs, 1.6M → 1.2M total stubs on Template, and 1.4M → 1.0M on Production per
invocation. Generated steady Map property hits remove 400,000 crossings; initial
OSR accounts for fractional averages over 28 invocations. All JIT runs produce
one code generation, with zero optimizing or generated-call deopts. Production
records 28 optimized entries. Template code is 7,668 → 7,972 bytes and
Production code 9,168 → 10,064 bytes in the benchmark context.

JIT compiler time totals, median across the three runs, are 0.233 → 0.123 ms
for Template and 2.953 → 3.540 ms for Production. These sub-millisecond changes
occur during warmup and do not explain the steady speedup. JIT managed
allocations remain 359 → 384 cells for Template and 461 → 486 for Production,
with the same 1,232-byte preparation increase and zero collections.

Interpreter retains exactly 33,600,311 managed allocations, 4,704,137,952 bytes
and 192 full/minor collections in every run. Full-GC pause totals range from
923.753–942.966 ms before and 936.554–946.193 ms after, across all 28 calls;
minor pauses range from 205.541–212.477 and 207.924–212.285 ms respectively.
Those unchanged collection counts and small pause variation do not account for
the Interpreter instruction reduction or wall gain. The earlier 1.83% wall
regression is resolved while retaining the moving-receiver correction.

The evidence now retains **90 run summaries and 1,800 individual wall samples**:
final pairs, the pilot, property-only candidates, isolation runs and noisy
controls. The final score excludes every intermediate series. At **538 gross
hand-written production LOC added**, wall-time benefit per added line is
**0.05930 percentage points for Production**, 0.03930 for Template and 0.01218
for Interpreter. This is a local kernel result, not an overall-engine claim.

## Validation

The final AArch64 focused matrix passed 32/32 test executions: eight tests at
each of GC stress unset, 16, 4 and 1. It covers all three execution tiers,
live replacement, accessors and throws exactly once, receiver overrides,
loaded-callable argument ordering, moving getter receivers and results,
snapshot preparation and the original 200,000-iteration kernel.
The same focused matrix passed 32/32 under Rosetta. The contiguous-string
correction also passed 62 VM string tests at each of GC stress unset, 16, 4
and 1 on both architectures: 248/248 per target. The x86-64 target-neutral
Machine suite passed 165/165.
The native benchmark kernel suite passed 11/11; the finalized installed-guard
fixture was then rerun successfully. Rosetta passed all three targeted
benchmark regressions. Warnings-denied all-targets/all-features Clippy passed
and was repeated successfully after the final test fixture adjustment.

Targeted Test262 on both AArch64 and Rosetta exactly matches the original
five sections: Map, Set, Object, String and `language/expressions/call`.
Each target reports 5,427 total, 5,420 pass, 7 skip, and zero failures, crashes,
timeouts or OOMs at the unchanged default timeout. The corpus commit is
`be13516fb6441b950ba8a3df97eb34062c186972`. An additional 49
`Function.prototype.call` tests also passed on AArch64; an initially mismatched
comparison filter was corrected to the exact saved baseline section.

Final native artifacts contain two 104-byte intrinsic receiver probes at byte
PCs 537 and 615, without calls or runtime-stub relocations inside them. The
captured CLI kernel body grows from 9,136 to 10,032 bytes. Its 19 safepoints
retain identical tagged-root and inline-frame recipes; the static inventory of
seven property cold siblings, five explicit calls and one `indexOf` method
call is unchanged. The kernel optimizes once with no deopt or bail.

`CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed in full
after the final String change (`gate-closed.log`):
formatting, warnings-denied Clippy, VM/JIT/bytecode tests and doctests, release
verifier/adversarial checks, 42/42 differential cases and the complete Template
and Production kernel ledger at eight warmups and twenty samples. Its
Production native-boundary result is 149.9812 ms, 1,000,045 total stubs,
199,999 property stubs and zero deopts. The preceding megamorphic property
kernel retains zero property stubs at 12.8439 ms. Final
`git diff --check` passed. No timeout or existing assertion was relaxed.

An independent production LOC audit counts **538 added Rust lines**: 353 in
tracked VM/JIT files, 97 in the new VM provider before its test module, and 88
in the new x86 helper. The count excludes benchmark instrumentation, all tests
and documentation files. Module comments and blank lines are included,
matching the preceding slice's counting method.

## Reproduction

Build `otter-engine-benchmark` with `cargo build --locked --release
-p otter-benchmark --features engine --bin otter-engine-benchmark`. Preserve
separate before/after executables. With `OTTER_GC_STRESS` unset, run each tier
serially, without concurrent builds or tests:

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/native-boundary.js \
  --function engineKernel --expected 27000000 \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

Wall samples exclude setup and warmup. Hardware instructions and cycles cover
the complete process, including setup and compilation. JIT/GC totals cover
warmups plus measured invocations; normalize counts by 28 when comparing one
invocation. Raw records, manifests, binary hashes, native profiles and compile
artifacts are retained locally under ignored
`benchmarks/results/native-boundary-2026-09-21/`.
