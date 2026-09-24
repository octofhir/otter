# Speculative shape-proven property loads

Investigation and closing validation: September 25, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`1a6da666c012cf6230add3a7ae22633865ec5152`. Development observations are not a
published engine baseline. Kernels are unmodified; hashes are in the
environment file.

## Starting point

A cost ledger over all 13 kernels showed three kernels where ProductionTiered
spent as many retired instructions as Template: `boxed-double-property`,
`method-call-monomorphic` and `string-concat`. For the first two, the
optimizing tier compiled cleanly and never exited, yet `boxed-double-property`
spent about 215 instructions per `box.value * box.scale` iteration.

The generated loop showed the cause. Every named load in the Machine tier was
a non-deopting CacheIR chain: `CacheIrGuardShape`, `CacheIrGuardAtomSlot` and
`CacheIrLoadField`, each of which decoded the receiver, checked the object
type and state bytes again, and fed a Boolean `active` chain into a
`TaggedSelect`/`BooleanOr` join, followed by a reentrant
`jit_load_property_value` cold call. That is about 75 instructions per load.
The cold call also made the whole loop an invalidating boundary, so LICM could
not move any of those proofs. `this.bias` inside the inlined `apply` of
`method-call-monomorphic` had the same shape.

V8 (CheckMaps), JSC (CheckStructure) and SpiderMonkey Warp (GuardShape) all
speculate on a monomorphic site and exit on a miss. Otter's earlier SsaOp tier
did the same before the Machine rebuild.

## Change

1. **Speculative monomorphic loads.** `property_speculation::speculated_load`
   admits a `LoadProperty` whose single baked program is `GuardShape`
   [+ `GuardAtomSlot`] + `LoadField` on the receiver, outside any local catch,
   not megamorphic and not an exotic `.length`. HIR emits one
   `PropertyShapeLoad` node, which does not end its block. Machine lowers it to:
   - `PropertyShapeProof`: one receiver decode, the object state checks, and
     the shape compare. It produces a Boolean with no incoming condition and no
     frame state.
   - `GuardCondition` with a `shapeGuard`/`recompile` exit at the pre-load
     state.
   - `PropertySlotLoad`: an unchecked slot read.

   The exit comes before any effect, so the interpreter performs the load
   exactly once. The exit is recorded per site (`optimized_exit_reasons`,
   including the innermost inlined callee site), and a recompiled site keeps
   the committed probe/cold-call form. Both AArch64 and x86-64 emit the two new
   operations.
2. **GVN** commons an identical dominating proof: the second load from the
   same receiver keeps only its slot read.
3. **LICM** no longer counts a loop header's `OsrEntry` as a write inside the
   loop. `split_preheader_and_hoist` already moves it to the front of the
   preheader, ahead of every hoisted instruction, so an OSR entry performs the
   hoisted proof itself. The proof now leaves a loop whose body writes no
   shape, descriptor or prototype state.

## Final paired result

Three directly alternating pairs per kernel and tier, eight warmups and twenty
samples, pair 2 reversed. The before binary is the previous slice's gate build
(`dc3e1d78…`, same source as the base commit). The after binary is this slice's
closing-gate build. All 54 processes validate their checksums. Load average was
about 3; no builds, tests or agents ran.

| Kernel / tier | Pair 1 before → after, ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| boxed-double-property / Production | 9.280 → 3.504 | 9.276 → 3.506 | 9.284 → 3.506 | **−62.23%** | **−71.91%** | −59.35% |
| method-call-monomorphic / Production | 8.561 → 4.639 | 8.570 → 4.640 | 8.571 → 4.643 | **−45.83%** | **−41.49%** | −43.44% |
| math-explicit-calls / Production | 7.392 → 4.492 | 7.377 → 4.462 | 7.375 → 4.457 | **−39.44%** | **−35.96%** | −37.35% |
| boxed-double-property / Template | 9.027 → 9.088 | 9.051 → 9.061 | 9.031 → 9.065 | +0.38% | +0.00% | +0.28% |
| method-call-monomorphic / Template | 7.967 → 7.985 | 7.965 → 7.978 | 7.968 → 7.979 | +0.17% | +0.00% | +0.23% |
| math-explicit-calls / Template | 11.649 → 10.897 | 11.649 → 10.886 | 11.647 → 10.891 | −6.49% | +0.00% | −6.22% |
| Interpreter (all three) | | | | +0.27% to +0.76% | −0.00% | +0.20% to +0.77% |

Template and Interpreter retire the same instructions, because neither tier
runs this code. The Template `math-explicit-calls` wall change comes with
identical instruction counts, so it is a code-placement effect and is not
claimed.

| ProductionTiered totals across 28 invocations (before → after) | boxed-double | method-call | math-explicit |
| --- | ---: | ---: | ---: |
| Optimizing deopts | 0 → 0 | 1 → 1 | 0 → 0 |
| Runtime property stubs | 0 → 0 | 0 → 0 | 0 → 0 |
| Emitted code bytes | 2,600 → 1,592 | 5,432 → 4,460 | 4,812 → 3,552 |
| Compiler wall time, ms | 1.25 → 0.92 | 1.36 → 0.90 | 1.33 → 0.86 |
| Allocated cells | 24 → 24 | 24 → 24 | 114 → 114 |

In `boxed-double-property`, the loop body goes from two full CacheIR chains
plus their cold join to one `cbz` on the hoisted proof and two slot reads:
about 215 → 60 instructions per iteration (gate ledger). The ledger also moves
`arith-exit-repair` by −20.5% (the `Math.abs` load), `derived-constructor` by
−7.2% and `native-boundary` by −5.3% in retired instructions. The other kernels
are flat within ±0.5%.

At **282 hand-written production Rust lines added** (14 removed, excluding
comments, tests, benchmarks and documentation), the wall gain per added line
is **0.221 percentage points for ProductionTiered on `boxed-double-property`**
(0.163 on `method-call-monomorphic`, 0.140 on `math-explicit-calls`).

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger.
- Machine suites: 174/174 (AArch64) and 169/169 (x86-64).
- Focused runtime matrix of 36 test binaries at `OTTER_GC_STRESS` unset, 16, 4
  and 1: 123/127 on AArch64 and 115/121 under Rosetta x86-64 at every level.
  Every failure predates this change and does not run the optimizing tier's
  property path:
  - `optimizing_leaf_deopt` ×3 and `optimizing_osr` ×1 on both targets: the
    Template tier compiles through OSR where the tests expect an entry compile.
  - `jit_private_access` and `jit_super_access` on x86-64 only: Template-tier
    selection, no optimizing compile.
- Targeted Test262 on AArch64 and Rosetta with zero failures, crashes,
  timeouts or OOMs in 14 sections. Map, Set, Object, String,
  `expressions/call`, Math, property-accessors, Reflect and `types/string`
  exactly match the previous slice's totals and failing sets. Newly covered:
  `expressions/object` 1170/1170, Proxy 275/311 (36 skipped), `expressions/delete`
  69/69, `expressions/assignment` 487/487 and `expressions/compound-assignment`
  454/454.
- `git diff --check` passed.

New regression `crates/otter-runtime/tests/jit_speculative_property_loads.rs`
(it fails on the old code, which emits no `machinePropertyShapeProof`):
- a warmed loop load is shape-proven and its proof is hoisted, with zero exits
  and zero property stubs;
- an OSR-entered loop performs the hoisted proof;
- a receiver of another shape exits at most twice over 800 calls;
- an accessor installed over the slot runs its getter exactly once per read;
- a loop that rewrites its receiver's shape keeps the proof inside the loop;
- a spliced callee (`this.bias`) exits at its own site.

Existing tests that encoded "a named load never exits" were updated to the new
contract without losing their purpose:
- Fixtures that exercise the committed cold path (getters and setters
  reentering from inlined bodies, CacheIR Boolean plumbing, accessor misses)
  now warm with two receiver shapes, which keeps those sites polymorphic
  CacheIR probes.
- Artifact helpers count shape-proven loads separately from committed
  siblings.
- The arrow test, whose captured receiver cannot vary, now expects exactly
  one exit.

## Reproduction

Build `otter-engine-benchmark` with `cargo build --locked --release
-p otter-benchmark --features engine --bin otter-engine-benchmark`, keep
separate before/after executables, and run each tier serially with
`OTTER_GC_STRESS` unset:

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/<kernel>.js \
  --function engineKernel --expected <checksum> \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-speculative-shape-loads-environment.json),
[run counters](2026-09-25-speculative-shape-loads-runs.csv) and
[wall samples](2026-09-25-speculative-shape-loads-samples.csv) are tracked
here. Binaries, logs, the baseline cost ledger and Test262 records are retained
under ignored `benchmarks/results/next2-2026-09-25/`.
