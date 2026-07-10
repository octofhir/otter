# JIT refactor Phase 0/1 evidence

Captured 2026-07-10. The required differential and GC-slot-verification gate is
now green. Phase 0 remains incomplete because macro suite reruns and the fresh
full Test262 matrix below are still outstanding; failed workloads still receive
no performance score.

## Environment

| Field | Value |
| --- | --- |
| Otter base commit | `d5b18165` (`main`, two commits ahead of `origin/main`; Phase 0/1 changes uncommitted) |
| Rust | `rustc 1.96.0 (ac68faa20 2026-05-25)`, LLVM 22.1.2 |
| Cargo | `cargo 1.96.0 (30a34c682 2026-05-25)` |
| OS | Darwin 25.5.0, macOS arm64 |
| CPU | Apple M1 |
| Build profile | `release`, thin LTO, one codegen unit, debug info 1 |
| Benchmark cache | V8 suite cache warm; CLI fixture inputs local; no persistent CodeBlock/module cache exists |

## Correctness baseline

The checked-in Test262 baseline remains the current full-corpus reference:

| Total | Passed | Failed | Skipped | Timeout | Crash | Pass rate excluding skips |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 53,173 | 51,219 | 752 | 1,191 | 11 | 0 | 98.53% |

It was captured at engine commit `6c2e9f793c71aaacb23050788480698657eda0fc`
and Test262 commit `7e115f46ac64340827d505fa928ad436cb7ba5a6`.
A new full Test262 matrix has not yet been run after the GC fixes.

The differential corpus contains eleven deterministic cases covering
arithmetic overflow/NaN/-0, calls/recursion, closures/upvalues,
property/prototype invalidation, arrays/typed arrays, exceptions/finally,
allocation, accessor/coercion ordering, microtasks, and explicit
RegExp/Iterator/Generator bootstrap surfaces. Interpreter-only is the oracle;
normal tiering, forced-baseline policy, and GC-stress processes are isolated and
wall-clock capped.

- Sparse matrix (`OTTER_GC_STRESS=1,4,16`): 8/8 passed before the exhaustive run.
- Exhaustive matrix (`1..16`): first exposed a stale bootstrap
  `Function.prototype` handle at strides 8–16. The handle is now rooted and
  reloaded after allocation.
- The bootstrap corruption was localized to unrooted native functions,
  reference-aliasing across GC safepoints, moving intrinsic iterator prototype
  caches, and an unrooted interval between `Interpreter` construction and the
  runtime dispatch loop. RegExp/Object/Iterator/Generator bootstrap now uses
  canonical root scopes and stable root cells; runtime class/extension/global
  installation runs inside a stationary whole-interpreter root scope.
- Per-kind iterator prototypes stay in the moving nursery and are rewritten by
  their stable root cells. Premature old-space allocation was removed because
  it converted their bootstrap methods into fragile old-to-young edges.
- The exhaustive differential result is now **11/11**. Every GC-stress
  candidate enables `OTTER_GC_VERIFY=1`; the complete stride matrix `1..16`
  passes with no stale root or heap-slot diagnostics. The former
  `arrays_typed.js`, `effect_order.js`, and `generator-bootstrap.js` blockers
  all pass. The typed-array stride-11 reproducer also passed 30 consecutive
  fresh processes.

Reproduce:

```bash
cargo build --release -p otter-cli
cargo run --release -p otter-difftest -- \
  --otter target/release/otter \
  --gc-strides 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16
```

## Fresh measurements

All values below are current-worktree measurements on the environment above.
Intervals are Criterion 95% confidence intervals. CLI values are 30-process
hyperfine samples and use the median because the host showed high-tail noise.

| Gate | Runtime/JIT mode | Result | Correctness |
| --- | --- | ---: | --- |
| 10,000-op NOP dispatch | VM interpreter | 50.412–50.758 µs | validated return |
| named property load+store, 1,000 iterations | VM interpreter, warm IC | 97.443–98.669 µs | validated return |
| prototype named load, 1,000 iterations | VM interpreter, warm IC | 77.687–78.648 µs | validated return |
| `Runtime::builder().build()` | in-process | 383.29–403.96 µs | constructed successfully |
| production sandbox runtime build | in-process | 393.40–407.25 µs | constructed successfully |
| `Otter::builder().build()` | in-process | 564.42–594.32 µs | constructed successfully |
| first tiny JavaScript runtime run | in-process | 408.91–432.04 µs | completion validated |
| first tiny TypeScript runtime run | in-process | 407.27–433.65 µs | completion validated |
| extracted `Math.abs` native call | in-process | 419.74–442.50 µs | completion validated |
| tiny JS cold process | interpreter-only | 7.932 ms median | exit/success validated |
| tiny JS cold process | normal baseline tiering | 7.654 ms median | exit/success validated |
| tiny TS cold process | interpreter-only | 8.556 ms median | exit/success validated |
| tiny TS cold process | normal baseline tiering | 7.766 ms median | exit/success validated |
| two-module ESM cold process | interpreter-only | 8.234 ms median | explicit value marker |
| two-module ESM cold process | normal baseline tiering | 9.297 ms median | explicit value marker; noisy 42 ms max |
| V8 v7 Richards, selected runner | normal baseline tiering | score 400 | suite success marker |

The machine-readable module slice separately times cumulative graph phases for
the validated two-module ESM fixture. Each row is the median of 20 release
samples. The fixture imports `value` from its leaf and throws unless it is 42.
`cold` constructs a fresh runtime per sample; `warm` reuses one runtime after
five validated pre-executions. Both states still rebuild the module graph from
source because no persistent CodeBlock/module cache exists. Consequently the
warm row represents reused runtime plus host filesystem state, not a cache hit.

| Runtime state | Wall | Resolve | Load | Parse | Compile/CodeBlock | Link/instantiate | Execute |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cold (fresh runtime/sample) | 178.458 µs | 79.083 µs | 30.770 µs | 3.395 µs | 21.249 µs | 19.333 µs | 13.437 µs |
| warm (persistent runtime, 5 warmups) | 147.625 µs | 64.937 µs | 26.937 µs | 2.000 µs | 14.792 µs | 13.979 µs | 9.979 µs |

Phase totals are intentionally cumulative over both modules and do not include
the small orchestration remainder visible in wall time. Runtime construction is
outside module wall time: the cold per-sample build median was 441.645 µs. The
warm command observed a single 704.333 µs build before warmup; that one noisy
setup observation is recorded in JSON but is not a cold/warm comparison.

The package-backed two-module fixture resolves `#phase0-dep` through a
checked-in `package.json#imports` map. It uses the same measurement boundaries
and correctness contract as the relative-module fixture while adding package
scope/manifest resolution. Each row is the median of 20 release samples.

| Package runtime state | Wall | Resolve | Load | Parse | Compile/CodeBlock | Link/instantiate | Execute |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cold (fresh runtime/sample) | 297.812 µs | 135.250 µs | 40.083 µs | 11.854 µs | 38.375 µs | 22.250 µs | 24.875 µs |
| warm (persistent runtime, 5 warmups) | 213.062 µs | 92.521 µs | 33.749 µs | 2.479 µs | 17.833 µs | 18.125 µs | 13.333 µs |

Runtime construction remains outside package wall time: the cold build median
was 555.145 µs. The warm command's one pre-warmup build was 1,073.334 µs and is
not used as a comparative result.

Focused call measurements below are sequential release runs, 20 medians after
three warmups. Each timed sample executes 100,000 calls from already parsed and
lowered bytecode in one persistent interpreter and checks the returned sum.
The totals include the surrounding JS loop/IIFE dispatch, so the normalized
column is a workload cost per completed call, not a standalone call-instruction
latency.

| Call shape | Interpreter-only total | Normal baseline total | Approx. ns/call (interp / baseline) |
| --- | ---: | ---: | ---: |
| bytecode arity 0 | 23.377 ms | 26.572 ms | 233.8 / 265.7 |
| bytecode arity 1 | 26.032 ms | 25.032 ms | 260.3 / 250.3 |
| bytecode arity 4 | 28.832 ms | 27.629 ms | 288.3 / 276.3 |
| bytecode arity 8 | 37.296 ms | 33.696 ms | 373.0 / 337.0 |
| extracted host `Math.abs` arity 1 | 24.651 ms | 26.028 ms | 246.5 / 260.3 |

Direct arities 256 and 1024 both produce a checked compiler diagnostic because
the current dense call encoding caps argument lists at 240. Their schema-v1
records have `success=false`, `validation=failed`, exit code 1, and no timing
score. They are compatibility gaps, not benchmark results. Baseline is not
claimed as universally faster from this matrix: arity 0 and the extracted host
call are slightly slower, while arities 1/4/8 improve in this workload.

The validated `phase0JitTarget` fixture tiers up during a 100-call semantic
check (`return=3300`) before its emitter samples are accepted. Across 100
release samples after ten warmups, direct baseline emission had a **6.479 us
median** and produced a stable **1,616-byte** finalized code buffer. Snapshot
construction and bytecode lowering are outside that timing; executable-buffer
finalization is inside it.

The managed-memory fixture allocates one array and one object per loop turn in
five fresh interpreter-only isolates. At 1,000,000 iterations its validated
medians were **591.819 ms JS execution**, **593.410 ms wall time including one
post-run forced full GC**, **2,000,004 GC allocations**, **31.264 ms cumulative
minor+full GC pause**, and **313,168 retained heap bytes** after full
reconciliation. Bootstrap allocations are excluded from the allocation and
pause deltas; retained heap includes the isolate bootstrap graph. Wrapping the
five-isolate process with opt-in 5 ms RSS sampling measured a **88,031,232-byte
peak RSS**. A new cumulative full-pause counter is updated only on the full-GC
slow path; RSS polling is out of process and opt-in, and no additional
allocation/dispatch hot-path counter was introduced.

Representative macro managed-memory/RSS measurements use the exact ordered V8
v7 source files and generated driver used by the CLI runner. They execute as
classic scripts in one CLI-equivalent runtime, capture the suite score marker,
then force a full GC. Each row is the median of five fresh-runtime release
samples with opt-in 5 ms self-RSS polling. Retained heap includes the full
runtime bootstrap graph and live benchmark state; GC time is the cumulative
minor+full pause delta after runtime construction, including the final forced
collection. Execution and GC times are not additive because in-workload GC
pauses are already inside execution time.

| V8 v7 workload | Wall incl. final GC | Execution | GC pause | Retained heap | Peak RSS | JIT buffers | Validation |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Richards (call/object/control) | 2,012.090 ms | 2,011.479 ms | 0.392 ms | 403,728 B | 36,798,464 B | 104,492 B | `Score (version 7): 377` |
| Splay (allocation/data structures) | 2,755.802 ms | 2,701.203 ms | 537.367 ms | 2,292,296 B | 502,251,520 B | 70,948 B | `Score (version 7): 1034` |

These are memory-residency observations, not same-commit performance
comparisons against another tier or runtime. The varying V8 scores are retained
only as semantic success markers; they are not used to claim a speedup.

Whole-runtime executable-code residency is queried only after each workload.
The snapshot deduplicates code-object identity across canonical entry/OSR maps,
the monomorphic entry cache, active direct-call anchors, and direct-method
caches. The byte count sums finalized native buffer lengths; it excludes Rust
metadata and executable mapping page-rounding overhead. This closes the former
single-buffer-only evidence gap without adding compilation/dispatch counters.

Post-GC-fix startup guard (same Criterion configuration): default runtime build
was 385.61–391.01 µs and an isolated rerun of production sandbox build was
392.22–408.23 µs, versus the pre-fix 383.29–403.96 µs and 393.40–407.25 µs
intervals above. The root-registration fix therefore shows no material startup
regression. First tiny JavaScript was 410.20–422.82 µs; tiny TypeScript was
426.28–445.82 µs. The full mixed run initially reported a noisy production
sandbox interval of 438.12–520.13 µs; the immediate isolated rerun returned to
the recorded band, so that outlier is not used as an architectural signal.

The documented 2026-07-09 reference was V8 v7 full-suite score 239 and
Richards 191 under the then-default optimizer-first selection. The fresh
Richards-only score is not a controlled before/after comparison: the base commit
and suite composition differ. It must not be used to claim a speedup. A clean
same-commit experimental-optimizer build/run remains required for that decision.

Commands:

```bash
cargo bench -p otter-vm --bench dispatch -- --noplot
cargo bench -p otter-vm --bench property_ic -- \
  'property_ic/(named_load_store_warm_1k|prototype_named_load_1k)' --noplot
cargo bench -p otter-runtime --bench startup -- --noplot
target/release/otter-phase0 call --kind bytecode --arity 4 \
  --jit-mode baseline --iterations 100000 --samples 20 --warmup 3
target/release/otter-phase0 jit-compile \
  --source benchmarks/fixtures/phase0/jit-compile.js \
  --function phase0JitTarget --expected 3300 --samples 100 --warmup 10
target/release/otter-phase0 memory --iterations 1000000 --samples 5
target/release/otter-phase0 module \
  --entry benchmarks/fixtures/phase0/module-entry.mjs \
  --cache-state cold --samples 20 --warmup 0
target/release/otter-phase0 module \
  --entry benchmarks/fixtures/phase0/module-entry.mjs \
  --cache-state warm --samples 20 --warmup 5
target/release/otter-phase0 module \
  --entry benchmarks/fixtures/phase0/package/entry.mjs \
  --cache-state cold --samples 20 --warmup 0
target/release/otter-phase0 module \
  --entry benchmarks/fixtures/phase0/package/entry.mjs \
  --cache-state warm --samples 20 --warmup 5
target/release/otter-phase0 macro-memory \
  --name v8-v7-richards-memory \
  --source benchmarks/.suite-cache/v8-v7/base.js \
           benchmarks/.suite-cache/v8-v7/richards.js \
           benchmarks/.suite-cache/v8-v7/driver.js \
  --validation-marker 'Score (version 7):' --samples 5 --rss-sample-ms 5
target/release/otter-phase0 macro-memory \
  --name v8-v7-splay-memory \
  --source benchmarks/.suite-cache/v8-v7/base.js \
           benchmarks/.suite-cache/v8-v7/splay.js \
           benchmarks/.suite-cache/v8-v7/driver.js \
  --validation-marker 'Score (version 7):' --samples 5 --rss-sample-ms 5
hyperfine --warmup 5 --runs 30 --export-json /tmp/otter-phase0-startup.json \
  'OTTER_JIT=0 target/release/otter benchmarks/fixtures/phase0/tiny.js' \
  'OTTER_JIT=1 target/release/otter benchmarks/fixtures/phase0/tiny.js'
OTTER_BENCH_TIMEOUT=60 benchmarks/run-v8-v7.sh richards
```

## Existing workload failure reproductions

The 2026-07-09 checked-in suite results remain reproduced classifications; no
failed workload is scored:

| Workload | Status | Classification |
| --- | --- | --- |
| ARES-6 Air | fail | early hash mismatch followed by `NOT_CALLABLE` |
| Web Tooling Babel | fail | 528-argument call exceeds current 240-argument compiler limit |
| yt-dlp/ejs TypeScript/ESM | fail | type-only `ESTree` import retained as a runtime import |
| Octane RayTrace | fail | `SIGSEGV` in documented run |
| Octane Box2D | fail | dynasm relocation panic in documented run |

Full V8/Octane, ARES, Web Tooling, and ejs have not yet been rerun after the GC
fixes. Detailed relative/package module graph phases, representative macro
managed heap/RSS, focused managed allocation, retained heap, GC pause, peak RSS,
single-buffer JIT size, and whole-runtime executable-code residency evidence is
now available above.

## Machine-readable results and telemetry overhead

`otter-benchmark` defines JSON schema version 1 and records commit, platform,
toolchain, profile, runtime/JIT/GC/cache modes, wall/phase times, code bytes,
allocation/GC/RSS/heap/code-memory counters, exit status, semantic validation,
command, and failure classification. Unavailable counters serialize as `null`.
Unvalidated or failed runs are deliberately non-scoreable.

```bash
cargo run -p otter-benchmark -- \
  --name tiny-js --runtime-mode cli --jit-mode interpreter-only \
  --gc-mode normal --cache-state cold --validation-marker ok -- \
  target/release/otter -p '"ok"'
```

No hot-path telemetry counter was added in this patch. The recorder is an
out-of-process wrapper and the opcode inventory is offline, so their disabled
runtime overhead is structurally zero. Module phase clocks and accumulation run
only through `Runtime::run_module_profiled`; ordinary `run_module` retains a
clock-free graph path. Macro memory reuses existing GC stats, invokes full GC
explicitly after validation, and starts its separate RSS sampler only when a
non-zero interval is requested. Whole-runtime JIT residency walks cold ownership
tables only on explicit request. Existing bootstrap/profiling telemetry remains
opt-in. A measured below-1% dispatch/V8 proof is still required before adding
opcode/stub/IC counters to the VM hot path.

## Opcode and VM/JIT ABI audit

`cargo run -p otter-bytecode --bin opcode-audit` emits one checked JSON row for
every active opcode byte. Coverage is derived from `OP_BYTE_TABLE`, and tests
reject missing/duplicate rows. Current effects are conservative; operations not
proven leaf are marked throw/allocate/GC/reentrant and safepoint-required.

The audit also records the central Phase 2 blocker honestly: the current
self-describing operand stream has no authoritative static opcode schema, so
exact register read/write sets remain consumer-decoded rather than generated.
Baseline support is marked partial/fallback and the old optimizer is marked
experimental-only. This inventory is a checked transitional audit, not the final
schema-generated wordcode table.

The passive native ABI now defines fixed-width C-layout `VmThread`,
`NativeFrameHeader`, `NativeFrame`, `DispatchStatus`, `DispatchResult`,
`RuntimeStubDescriptor`, `SafepointEntry`, `FrameMap`, `SpillMap`, and
`CodeDependency` records plus VM/stub/build versions. Golden host tests assert
sizes and offsets. Runtime stubs carry explicit effects; leaf descriptors reject
allocation, GC, throw, reentry, and safepoints, while allocating/reentrant stubs
require a safepoint. The current 17 descriptors form a dense checked inventory.

Code lifetime is explicit: invalidate and unlink entry first, retain active
code anchors until no frame can return, then retire/reclaim. Derived object,
slab, string, array-buffer, upvalue, backing-store, or feedback pointers may not
survive a safepoint; only declared tagged frame/spill roots may do so.

## Optimizer and temporary compatibility paths

The old optimizer is absent from default builds and default tier selection.
It is available only with Cargo feature `experimental-optimizer` and runtime
switch `OTTER_EXPERIMENTAL_OPTIMIZER=1`. Its Cranelift dependencies are optional.
No opcode coverage or performance work was added.

Temporary compatibility code and deletion conditions:

| Temporary path | Deletion condition |
| --- | --- |
| `JitFunctionView` and layout offsets | Phase 2 immutable CodeBlock request plus differential parity |
| `RuntimeStubAllocContext` raw `Interpreter`/stack/context pointers | unified `VmThread`/`NativeFrame` adopted by interpreter and baseline |
| `SafepointRecord { Vec<TaggedLocation> }` | CodeObject-owned `SafepointEntry` + frame/spill tables walk current JIT tests |
| byte-PC fields in native frame | schema wordcode uses instruction-index PC and source-PC side tables |
| `optimizing/*` feature | same-commit evidence/regression fixtures extracted and baseline v2 contracts accepted |
| manual bootstrap value roots | handle-scope/static builder migration covers all bootstrap allocations under stress 1..16 |

## Cache-key evidence for Phase 2

A future content-addressed CodeBlock/module cache key must include source content
hash, compiler/schema version, Otter build and VM layout version, target/endian
where native metadata participates, source kind (JS/TS/JSX and module/script),
transform flags, tsconfig/JSX settings, package conditions, referrer/resolved URL,
import-map/lockfile/package-manifest identity, capability-sensitive hosted-module
surface, and source-map/original-source hash. Corrupt/mismatched entries must
atomically fall back to parse/compile and never weaken permission checks.

## Required next sequence

The differential/GC prerequisite for Phase 2 is satisfied. Before presenting
the Phase 2 migration decision, complete the remaining Phase 0 evidence:

1. Run full VM/runtime/Test262 and same-commit baseline/experimental-optimizer
   performance matrices; complete module/package/macro and whole-runtime
   residency evidence.
2. Replace the conservative opcode audit fields with one declarative schema that
   generates exact formats, register reads/writes, successors, effects, verifier,
   disassembler metadata, and tier policy.
3. Introduce immutable verified `CodeBlock` identities with instruction-index
   PCs, constants, exception/source tables, block/loop metadata, and feedback
   layout; keep the old DTO only as an off-hot-path input adapter.
4. Add a reservation-stable segmented register stack and make interpreter frames
   populate the authoritative C-layout header; keep async/generator cold state by
   stable index.
5. Adapt the interpreter to new CodeBlocks behind the compatibility reader and
   run old/new interpreter differential over the complete passing Test262 set,
   async/generator/exception cases, and GC stress.
6. Add dense traced feedback vectors shared by interpreter and later baseline;
   remove per-site hash maps only after IC/invalidation telemetry shows parity.
7. Switch execution atomically after correctness, metadata-size, and >=1.5x
   dispatch gates; then delete the self-describing frozen execution form,
   byte-to-instruction map, and compatibility adapter.
8. Only then delete/rebuild baseline machine code over the stable CodeBlock,
   frame, stub, status, safepoint, and dependency contracts.
