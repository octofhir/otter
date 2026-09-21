# Generated megamorphic named stores — 2026-09-21

Local dirty-worktree observations on Apple M1/macOS ARM64 at
`e4c9e18048088e3cc1dd9f198db8a9fc6e580f06`; not a published baseline.
The comparison uses the final property result in
[`2026-09-20-jit.csv`](2026-09-20-jit.csv) as BEFORE and the same source,
checksum, tier, warmup count and sample count as AFTER.

## Cause and implementation

The optimizing property kernel retained one `jit_store_property_value` call per
loop iteration after megamorphic loads became generated. Six receiver shapes had
already disabled the four-entry site PIC. The isolate-wide 1,024-entry
shape/atom table knew the current slot but did not expose descriptor writability,
so Machine could not prove that a direct store was legal.

Each shared entry now carries the immutable writable bit from the same resolved
own data descriptor. `PropertyMegamorphicStore` validates the full atom,
receiver and holder shape, ordinary fast state, own depth, data kind,
writability, slot identity and live storage bounds before its only effect. The
existing Machine write barrier handles young values. Any failed proof enters the
existing rooted committed store once. An optimizing megamorphic cold store
publishes its final proven own slot into that same table, so standalone
write-only sites do not depend on a later load to seed generated execution.
Template stores keep their existing per-site path and do no extra shared-table
work. No new cache, runtime
ABI, deopt/replay path, compatibility mode or second property contract was added.

The generated hit is O(1): one direct-mapped table probe plus fixed guards and
one slot write. The previous hot path was also O(1), but crossed the generated
code/Rust boundary and repeated canonical property machinery for every store.

## Release measurement

Native AArch64 release, normal GC, 8 warmups + 20 samples, no concurrent
builds/tests. Wall medians exclude warmup; process hardware counters include
setup and JIT compilation.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Wall median | 46.921354 ms | 12.470479 ms | **3.763×**, −73.42% |
| Property runtime stubs / invocation | 399,971.786 | 0 | −100% |
| Process retired instructions | 21,832,444,636 | 7,945,655,676 | −63.61% |
| JIT compiler wall total | 3.607876 ms | 3.194625 ms | −11.45% |
| Emitted code bytes | 11,440 | 12,164 | +724 bytes |
| Code generations / optimized deopts | 2 / 1 | 2 / 1 | unchanged |

The validated checksum remains `80011800000`. Relative to the original
103.863834 ms measurement before the preceding property package, the cumulative
speedup is 8.329×. This is a specialized kernel result, not an overall-engine
claim.

An intermediate layout placed the writable bit in `AtomOwnPropertyHit`, a hot
carrier used by Template property resolution. Three alternating 10-sample/4-
warmup pairs exposed a repeatable regression: signed-baseline medians
77.63/77.82/77.99 ms versus 97.40/97.28/97.80 ms. The final layout uses existing
padding in the shared table entry instead. Repeating three pairs produced
78.76/78.47/78.16 ms for the baseline and 78.90/78.76/78.83 ms for the final
source; median-of-medians differs by 0.46%. Stub counts were identical. This
keeps writable metadata off the Template hot carrier rather than accepting or
hiding a tier regression.

Artifact capture at the final source shows four symbolic
`propertyLookupCacheTable` relocations. The store site at byte PC 259 has one
`machineMegamorphicPropertyStore`, one generated write barrier and one
`machinePropertyStoreCold` sibling.

## Semantic and target checks

The focused runtime suite seeds a write-only site and checks own writable hits,
a young stored object across
moving collection, non-writable data properties, accessors and effect-once
setter behavior. It passes with GC stress unset and strides 16, 4 and 1. Both
AArch64 and x86-64 select, allocate and execute the same six-test suite; x86-64
runs under Rosetta. Target-neutral Machine tests require one generated store,
one barrier, one committed cold call, the complete effect row and no safepoint,
throw or reentry on the hit.

The final-source `built-ins/Object/` Test262 selection passes 3,413 executable
tests with one configured skip on each target: zero failures, timeouts, OOMs or
crashes on native AArch64 and x86-64 under Rosetta.

The closing `scripts/gate.sh` run passed formatting, warnings-denied workspace
Clippy, debug/unit/compile-fail/doctest and release adversarial checks. Its
interpreter/tier/GC-stress differential matrix passed 42/42 cases. The final
20-sample kernel ledger reports 78.3180 ms and 1,599,999 property stubs for
Template versus 13.0666 ms and zero property stubs for ProductionTiered; the
independent paired run above is the before/after performance comparison.
`git diff --check` also passes.
