# Declared leaves at explicit-receiver calls

Investigation and closing validation: September 24, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`d99630e8c2ce1695a8d9fa3d7442df647c8f9cc4`. Development observations are not a
published engine baseline. The unmodified `benchmarks/scripts/native-boundary.js`
has SHA-256 `60bb4c98c42472fc2db9473646f6b54c9990479fc1870a5ded93e5706b54faf4`
and returns exactly `27000000` on every invocation.

## Remaining crossings

After the `indexOf` slice, every remaining native crossing in the kernel sits
at an explicit-receiver call. The callee is loaded first and the arguments are
evaluated after it, so these compile to `LoadProperty` plus `CallWithThis`:

| Site (byte PC) | Template before | ProductionTiered before |
| --- | --- | --- |
| `word.charCodeAt` load (224) | cold property load | cold property load |
| `word.charCodeAt(index & 3)` (250) | general call | general call |
| `Math.abs(…)` / `Math.max(…)` (408 / 484) | 2 × general call | generated Int32 hits |
| `table.set(…)` (573) | general call | general call |
| `table.get(…)` (641) | general call | general call |

Per warmed invocation that is 1,000,000 JIT-to-Rust transitions on Template
and 600,000 on Production, plus 200,000 property stubs on both.

The cause was the static-native registry (`jit_static_native`), which declared
only `Math` and `parseInt`. `charCodeAt` and Map `get` already have
allocation-free leaf entries, but call feedback could not name them. Template
AArch64 also filtered every explicit-receiver site out of its static-leaf path
(`receiver.is_none()`), so even the declared `Math` calls took the general
boundary there.

## Selected implementation

One registry change feeds both tiers. `JitLeafBuiltin` gains `this_operand`:
the entry reads the call's `this` as its first operand word and proves its
type itself. New declarations cover String `charCodeAt`, `codePointAt`,
`indexOf`, `includes`, `startsWith` and `endsWith`, Map `get` and `has`, and
Set `has`. Their identities come from the same bridge functions the method
snapshots already use; `bootstrap_collections` now exposes its Map/Set
builtins through one function each instead of inline tables.

- **Machine:** `resolved_hit` replaces the Int32-only `supports_resolved`. A
  resolved `CallWithThis` guards the already-loaded callee with the existing
  `NativeLeafIdentity`, then runs the existing `NativeLeafProbe` with `this`
  as word 0 when the declaration reads it. Int32 `Math` with non-Int32
  operands now takes the tagged leaf instead of the general call. A miss
  enters the committed call sibling once.
- **Template AArch64:** an explicit-receiver site with static-native feedback
  runs the identity guard and `emit_native_entry_call` with `this` first when
  declared. A miss falls into the generic call transition, not a bail, so a
  leaf that keeps missing never causes an interpreter re-entry loop.
- **Consumers that pass no receiver word** (plain `Op::Call` leaves on both
  tiers, shaped `CallMethodValue` feedback) reject `this_operand`
  declarations. Plain calls and shaped method sites therefore keep the
  ordinary call for them.

No second call ABI or duplicate declaration table is added. The leaf family,
`NativeLeafProbe`, identity guard and committed siblings all already existed.
The x86-64 Template tier keeps its general call for explicit receivers, as it
already did for guarded method calls; Machine lowers the new sites on both
targets.

## Final paired result

Three directly alternating pairs per tier, eight warmups and twenty samples,
pair 2 reversed. The before binary is the previous slice's gate build
(`add8bc03…`). The after binary is byte-for-byte this slice's closing-gate
build (`29b0cbd9…`). No builds, tests or agents ran during the series. All 18
processes validate checksum `27000000`.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 344.567 → 341.939 | 339.384 → 340.010 | 339.255 → 340.815 | **−0.04%** |
| Template | 131.791 → 61.301 | 132.332 → 61.373 | 132.148 → 61.347 | **−53.56%** |
| ProductionTiered | 97.365 → 63.467 | 97.567 → 63.807 | 97.417 → 63.363 | **−34.79%** |

| Paired process counters | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Retired instructions | −0.01% | −49.86% | −27.98% |
| Hardware cycles | +0.05% | −53.21% | −34.40% |

| Totals across 28 invocations | Template before → after | ProductionTiered before → after |
| --- | ---: | ---: |
| JIT-to-Rust transitions | 27,999,960 → 5,599,992 | 16,799,952 → 5,600,002 |
| Runtime stub transitions | 33,601,346 → 11,201,378 | 22,401,294 → 11,201,344 |
| Property stubs | 5,599,992 → 5,599,992 | 5,599,975 → 5,599,975 |
| Optimizing / generated-call deopts | 0 / 0 → 0 / 0 | 0 / 0 → 0 / 0 |
| Emitted code bytes | 7,980 → 8,348 | 10,540 → 10,908 |
| Managed cells / bytes | 384 / 145,904 → same | 486 / 160,184 → same |
| Full / minor collections | 0 → 0 | 0 → 0 |

Template removes 22,399,968 crossings (800,000 per warmed invocation:
`charCodeAt`, `Math.abs`, `Math.max` and Map `get`). Production removes
11,199,950 (400,000 per invocation: `charCodeAt` and Map `get`; its `Math` hits
were already generated). Both tiers now cross only for `table.set`, which may
insert and allocate, and one `new Map()` per invocation; they also still pay
200,000 `charCodeAt` property loads. Production compile-time medians are
3.54 → 3.31 ms, outside the timed samples. The Interpreter is unchanged: its
allocation, byte and collection totals are identical.

After this slice Production (63.5 ms) is about 3.5% slower than Template
(61.3 ms). Both run the same leaf calls; Production spends its residual
difference elsewhere in the kernel, and this slice makes no claim about
that difference.

At **278 gross hand-written production LOC added** (49 removed), the wall gain
per added line is **0.1251 percentage points for ProductionTiered** and
0.1927 for Template (`loc-audit.txt`; tests, inline test modules, benchmarks
and documentation excluded).

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source (`gate-closed.log`): fmt, warnings-denied Clippy, unit
  tests, release verifier/adversarial corpus, 42/42 differential cases, and
  the kernel ledger (Template 63.90 ms, 400,049 stubs; Production 65.38 ms,
  400,048 stubs; zero deopts).
- Focused runtime matrix across 14 test binaries: 58/58 on AArch64 and 53/53
  under Rosetta x86-64 at each of `OTTER_GC_STRESS` unset, 16, 4 and 1. The
  Rosetta run is smaller because some tests are AArch64-only.
- Target-neutral Machine suites 173/173 (AArch64) and 168/168 (x86-64);
  registry unit tests 2/2.
- Targeted Test262 on AArch64 and Rosetta. Map, Set, Object, String and
  `language/expressions/call` exactly match the previous slice's totals and
  failing sets: 5,427 total, 5,420 pass, 7 skip, zero failures. Because `Math`
  calls now lower too, `built-ins/Math` was added: 327/327 on both targets,
  and the ES_CONFORMANCE failing set has no Math entry.
- `git diff --check` passed.

New `jit_resolved_native_leaves` regressions cover warm leaf hits with zero
transitions, replaced callees, wrapper and rope receivers, out-of-range and
non-Int32 indices, Map subclasses, foreign receivers throwing `TypeError`
once, plain calls and shaped method sites never lowering `this`-reading
declarations, zero deopts on numeric leaf misses, and moving collection
between hits. `optimizing_map_intrinsics` now asserts the current contract
(Map `get` calls its declared leaf) instead of the removed legacy inline
intrinsic.

### Pre-existing failures outside this slice

A wider run of runtime tests, which the gate does not include, found failures
in fixtures that never call a declared leaf builtin. Each depends on tier-up
or OSR thresholds, not on the call paths changed here:

- `jit_stack_owned_array_construct` (3 tests; missing Machine constructor bundles)
- `jit_machine_generic_elements::cold_generic_value_calls_preserve_direct_hot_sites_and_execute_effects_once`
- `jit_stack_owned_runtime_families::direct_generated_runtime_families_match_interpreter_without_deopt`
- `jit_load_and_coercion::method_resolution_error_does_not_replay_observable_get` (a 12-iteration loop no longer reaches OSR)
- `gc_reentry_collections::native_set_expando_value_survives_allocation_and_full_gc` (no Template compile)
- `jit_inlined_numeric_leaf::production_inline_full_gc_and_nested_abrupt_exit_stay_reusable`

They are recorded here and left for a separate fix. A fixture exploration
also showed one Machine arithmetic behaviour: a loop computing `-(0) * 0.5`
logged 42 `negativeZero` bails in one run. That is Int32 speculation, not a
leaf exit, and the regression fixture avoids `-0` for that reason.

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

[Environment](2026-09-24-resolved-native-leaf-environment.json),
[run counters](2026-09-24-resolved-native-leaf-runs.csv) and
[wall samples](2026-09-24-resolved-native-leaf-samples.csv) are tracked here.
Binaries, logs, artifacts and Test262 records are retained under ignored
`benchmarks/results/resolved-leaf-2026-09-24/`.
