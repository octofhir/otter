# JIT refactor Phase 0/1 evidence

Captured 2026-07-10. The differential/GC-slot-verification matrix, detailed
module/package timings, representative macro memory/RSS, whole-runtime JIT code
residency, macro suite reruns, same-commit optimizer comparison, and fresh full
Test262 matrix are complete. Phase 0 evidence is closed. Failed or unvalidated
workloads receive no performance score. The explicit Phase 2 decision and its
entry conditions are recorded at the end of this document.

## Environment

| Field | Value |
| --- | --- |
| Otter evidence-head commit | `56bc0e56` (`main`; Phase 0/1 evidence commit `7c00a748` is in its history) |
| Rust | `rustc 1.96.0 (ac68faa20 2026-05-25)`, LLVM 22.1.2 |
| Cargo | `cargo 1.96.0 (30a34c682 2026-05-25)` |
| OS | Darwin 25.5.0, macOS arm64 |
| CPU | Apple M1 |
| Build profile | `release`, thin LTO, one codegen unit, debug info 1 |
| Benchmark cache | V8 suite cache warm; CLI fixture inputs local; no persistent CodeBlock/module cache exists |

## Correctness baseline

The final exact full-corpus rerun on the evidence-head commit is the current
Test262 reference:

| Total | Passed | Failed | Skipped | Timeout | Crash | Pass rate excluding skips |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 53,173 | 51,480 | 498 | 1,185 | 10 | 0 | 99.02% |

It was captured at engine commit
`56bc0e56b86c668a919a302b5496063ecf3eab97` and Test262 commit
`7e115f46ac64340827d505fa928ad436cb7ba5a6`. Relative to the pre-Phase-0
checked-in reference, this is +261 passing, -254 failing, -6 skipped, and -1
timeout. The first exact full run at this commit reported one worker `SIGSEGV`
at `staging/sm/Date/two-digit-years.js`; the test then completed as an ordinary
known failure in 10/10 fresh isolated reproductions, and the second exact full
run completed with zero crashes. Both observations are retained: the clean
second run is the canonical baseline, while the first crash remains an
instability signal rather than being reclassified as a passing result.

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
Richards 191 under the then-default optimizer-first selection. It is not used
for a speedup claim because the commit and tier policy differ. A controlled
same-commit comparison at `56bc0e56` completed on the exact full V8 v7 suite:

| Tier policy | Richards | DeltaBlue | Crypto | RayTrace | EarleyBoyer | RegExp | Splay | NavierStokes | Composite |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| default baseline JIT | 335 | 246 | 96.2 | 357 | 606 | 209 | 1,139 | 144 | **296** |
| `experimental-optimizer` + runtime opt-in | 390 | 243 | 95.5 | 340 | 556 | 152 | 843 | 138 | **272** |

The experimental optimizer is 8.1% lower by composite score and therefore
fails the performance gate. It remains excluded from default builds and is not
a migration base for Phase 2. The first default full-suite attempt at this same
commit exited with `SIGSEGV` while starting RayTrace after three validated
workloads and received no composite score. An immediate isolated RayTrace run
validated in both baseline-JIT (324) and interpreter-only (328) modes, and the
subsequent exact default full-suite rerun produced the scoreable 296 result
above. The failed attempt remains a nondeterministic stability observation.

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
OTTER_BENCH_TIMEOUT=0 benchmarks/run-v8-v7.sh
OTTER_BENCH_TIMEOUT=0 OTTER_EXPERIMENTAL_OPTIMIZER=1 \
  benchmarks/run-v8-v7.sh
TIMEOUT=10 bash scripts/test262-full-run.sh
target/release/otter-test262 conformance test262_results/latest.json \
  --output ES_CONFORMANCE.md
OTTER_BENCH_TIMEOUT=60 benchmarks/run-octane.sh
OTTER_BENCH_TIMEOUT=60 benchmarks/run-ares6.sh
OTTER_BENCH_TIMEOUT=60 benchmarks/run-web-tooling.sh --only babel
OTTER_BENCH_TIMEOUT=60 benchmarks/run-ejs.sh
```

## Fresh macro suite reruns

All macro suites were rerun after the GC fixes with the release CLI and their
checked-in runner semantics unchanged. A result is scoreable only when the
runner's semantic marker and process status both validate.

| Suite/workload | Fresh outcome | Scoreability/classification |
| --- | --- | --- |
| V8 v7 full suite | validated default-JIT retry, composite 296 | scoreable; the preceding `SIGSEGV` attempt is separately retained as non-scoreable |
| Octane Richards | 393 | scoreable workload result |
| Octane DeltaBlue | 241 | scoreable workload result |
| Octane Crypto | 93.6 | scoreable workload result |
| Octane RayTrace | 338 | scoreable workload result; former crash blocker cleared |
| Octane EarleyBoyer | 589 | scoreable workload result |
| Octane RegExp | 154 | scoreable workload result |
| Octane Splay | 1,900 plus valid latency marker | scoreable workload result |
| Octane NavierStokes | 144 | scoreable workload result |
| Octane PDFJS | 491 | scoreable workload result |
| Octane GBEmu | 908 | scoreable workload result |
| Octane CodeLoad | 4,778 | scoreable workload result |
| Octane Box2D | 553 | scoreable workload result; former relocation blocker cleared |
| Octane Mandreel | compiler rejects the 65,535-register window | non-scoreable compatibility limit |
| Octane zlib | `ReferenceError: print is not defined` | non-scoreable missing host surface |
| Octane TypeScript | abort in `jit_store_prop_stub` after an object-slot bounds panic | non-scoreable JIT correctness/stability failure |
| ARES-6 Air | early hash mismatch followed by `NOT_CALLABLE` | non-scoreable semantic failure; suite has no numeric summary |
| Web Tooling Babel | 528-argument call exceeds the 240-argument compiler limit | non-scoreable compatibility limit |
| yt-dlp/ejs | `ESTree` import from `meriyah` does not resolve an exported runtime binding | non-scoreable module-compatibility failure |

Octane has no aggregate score because three workloads fail. The V8 first-run
`SIGSEGV`, Octane TypeScript abort, and both Test262 full-run observations are
kept visible; successful retries do not rewrite failed attempts into scores.

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

The audit now projects every active opcode from one authoritative schema:
identity, byte assignment, fixed or counted-variadic operands, exact register
reads/writes, normal and exceptional successors, verifier metadata, feedback
family, effects, and tier policy. There is no legacy-only or
consumer-decoded opcode class. Baseline support remains partial/fallback and
the old optimizer remains experimental-only; neither execution tier changed in
this schema slice.

The passive native ABI now defines fixed-width C-layout `VmThread`,
`NativeFrameHeader`, `NativeFrame`, `DispatchStatus`, `DispatchResult`,
`RuntimeStubDescriptor`, `SafepointEntry`, `FrameMap`, `SpillMap`, and
`CodeDependency` records plus VM/stub/build versions. Golden host tests assert
sizes and offsets. Runtime stubs carry explicit effects; leaf descriptors reject
allocation, GC, throw, reentry, and safepoints, while allocating/reentrant stubs
require a safepoint. The current 56 descriptors form a dense checked inventory;
the structured-exception transition is descriptor 56 and table version 4.

Code lifetime is explicit: invalidate and unlink entry first, retain active
code anchors until no frame can return, then retire/reclaim. Derived object,
slab, string, array-buffer, upvalue, backing-store, or feedback pointers may not
survive a safepoint; only declared tagged frame/spill roots may do so.

## Deleted optimizer and remaining migration paths

The experimental Cranelift optimizer, its runtime switch, feature flag,
dependencies, bridge-only stubs, and `optimizing/*` source tree were deleted
after the measurement above showed no reason to preserve a parallel compiler.
There is now one active native compiler path.

Temporary compatibility code and deletion conditions:

| Temporary path | Deletion condition |
| --- | --- |
| `RuntimeStubAllocContext` raw `Interpreter`/stack/context pointers | unified `VmThread`/`NativeFrame` adopted by interpreter and baseline |
| `SafepointRecord { Vec<TaggedLocation> }` | CodeObject-owned `SafepointEntry` + frame/spill tables walk current JIT tests |
| byte-PC fields in native frame | schema wordcode uses instruction-index PC and source-PC side tables |
| manual bootstrap value roots | handle-scope/static builder migration covers all bootstrap allocations under stress 1..16 |

## Cache-key evidence for Phase 2

A future content-addressed CodeBlock/module cache key must include source content
hash, compiler/schema version, Otter build and VM layout version, target/endian
where native metadata participates, source kind (JS/TS/JSX and module/script),
transform flags, tsconfig/JSX settings, package conditions, referrer/resolved URL,
import-map/lockfile/package-manifest identity, capability-sensitive hosted-module
surface, and source-map/original-source hash. Corrupt/mismatched entries must
atomically fall back to parse/compile and never weaken permission checks.

## Phase 2 gate decision and required next sequence

**Decision: GO for Phase 2, with the evidence baseline frozen at
`56bc0e56`.** Phase 0 is complete: correctness, differential GC stress,
startup/call/JIT/module/package timing, managed heap/RSS, whole-runtime native
code residency, macro compatibility, and same-commit tier evidence are all
recorded. The subsequent status sections record the completed schema/CodeBlock
migration and default-tier switch. Failed or unvalidated workloads still receive
no performance score.

Entry constraints for every Phase 2 vertical slice:

- Keep one production template JIT. The legacy baseline emitter and experimental
  optimizer have been deleted; neither may return as a compatibility stack.
- Preserve the machine-readable schema-v1 fixtures and semantic markers. Failed
  or unvalidated macro workloads remain non-scoreable.
- Treat the observed V8 `SIGSEGV` and Octane TypeScript JIT abort as stability
  regression sentinels. A tier switch cannot proceed while either reproduces on
  the candidate tier.
- Maintain 11/11 differential parity with `OTTER_GC_STRESS=1..16` and
  `OTTER_GC_VERIFY=1`, green VM/runtime suites, and zero crashes in the canonical
  full Test262 run. Re-run the proportional subset after each slice and the full
  matrix before an execution-form or tier switch.
- Keep new telemetry default-off and out of dispatch/allocation hot paths unless
  a separately measured overhead gate permits it.

The completed Phase 2 implementation order was:

1. Replace the conservative opcode audit fields with one declarative schema that
   generates exact formats, register reads/writes, successors, effects, verifier,
   disassembler metadata, and tier policy.
2. Introduce immutable verified `CodeBlock` identities with instruction-index
   PCs, constants, exception/source tables, block/loop metadata, and feedback
   layout; keep the old DTO only as an off-hot-path input adapter.
3. Add a reservation-stable segmented register stack and make interpreter frames
   populate the authoritative C-layout header; keep async/generator cold state by
   stable index.
4. Adapt the interpreter to new CodeBlocks behind the compatibility reader and
   run old/new interpreter differential over the complete passing Test262 set,
   async/generator/exception cases, and GC stress.
5. Add dense traced feedback vectors shared by interpreter and later baseline;
   remove per-site hash maps only after IC/invalidation telemetry shows parity.
6. Switch execution atomically after correctness, metadata-size, and >=1.5x
   dispatch gates; then delete the self-describing frozen execution form,
   byte-to-instruction map, and compatibility adapter.
7. Rebuild the production template machine code over the stable CodeBlock,
   frame, stub, status, safepoint, and dependency contracts, then delete the
   parallel emitters. This is complete; current work removes measured bailouts
   from that single production tier.

### Phase 2 slice 1 status

The first minimal schema slice is implemented in `otter-bytecode`. One
declarative table now owns opcode identity/byte assignment and generates the
unchanged `OP_BYTE_TABLE` compatibility view. The same schema owns the current
self-describing operand-format classification, conservative effects and
control-flow classes, feedback family, and baseline/experimental tier policy.
The machine-readable opcode audit exposes an authority object per row so
schema-authoritative, schema-conservative, and transitional consumer-decoded
fields cannot be confused.

Item 1 is complete for the active opcode set. Every fixed and counted-variadic
layout has exact operand and register roles. Normal CFG metadata covers jumps,
branches, returns, tail-call fallback, calls, exception-region fallthrough, and
suspend/resume continuations. Exceptional CFG metadata covers optional encoded
handlers, dynamic frame/caller unwind, parked abrupt completion, and
finally-floor unwinding. The former `ConsumerDecoded` shape/status variants and
their audit fallback text were deleted; a new opcode cannot enter the table
without an exhaustive schema row.

`Op::operand_count`, decoder shape verification, whole-function target-boundary
verification, `OP_BYTE_TABLE`, and machine-readable audit projections consume
the schema. `JumpViaFinally` is also included in the shared branch fixup,
eliminating a concrete encoder/executable-builder drift. Disassembler rendering,
interpreter dispatch, baseline execution, experimental optimizer, bytecode ABI,
GC, and safepoint behavior remain unchanged.

The next Phase 2 slice has started in place: the former VM
`ExecutableFunction` is now the single immutable `CodeBlock` type used by
dispatch, frames, call helpers, and JIT snapshot construction. CodeBlock
construction stores the authoritative schema-verified decoder result, so the
duplicate VM-side branch-operand translator was deleted. Invalid compiler DTOs
fail CodeBlock construction instead of entering execution. There is no type
alias, parallel execution stack, or legacy opcode bucket.

The canonical PC migration is complete for frames, interpreter dispatch,
exception regions, property IC lookup, baseline branches, and inline bodies.
Native compilation now receives a `JitCompileSnapshot` whose instruction
metadata holds the exact immutable `Arc<CodeBlockInstruction>` records executed
by the interpreter. `JitFunctionView`/`JitInstrView` were deleted without
aliases: opcode and operands are no longer copied into a parallel JIT DTO;
dynamic feedback and layout data remain a separate compile-time overlay.

### Phase 2 Richards development sentinel

The canonical frame/CodeBlock PC and baseline branch lowering now use dense
instruction indexes. Serialized byte PCs remain side metadata for source maps,
profiling, OSR, and bailout records; interpreter dispatch no longer performs a
byte-PC-to-instruction lookup. A five-run release Richards sentinel with
`OTTER_JIT=1` moved from `269/324/335/356/373` (median `335`) before the PC
migration to `319/353/353/355/355` (median `353`) after the baseline emitter was
converted to the same instruction-index CFG, approximately `+5.4%`. This is a
local development signal, not a replacement for the frozen full-suite evidence
or a claim about other workloads.

After deleting the parallel optimizer and switching JIT instruction metadata
to shared `CodeBlockInstruction` records, the same five-run release sentinel
scored `338/365/349/359/357` (sorted median `357`): approximately `+1.1%`
versus the preceding `353` checkpoint and `+6.6%` versus the pre-migration
median `335`. Higher is better; wall times were `2.77/2.04/2.02/2.02/2.10 s`,
with the first run carrying the expected cold-process/link-cache noise.

### Phase 2 production StoreProperty miss completion

The production template tier now completes every named `StoreProperty` miss in
place through the VM's shared value-level `[[Set]]` implementation. The inline
own-data and ordinary shape-transition paths are unchanged. Inherited setters,
throwing setters, inherited non-writable data, Proxy and exotic receivers,
primitive receivers, megamorphic sites, and reentrant allocation no longer use
the store stub's exact side exit. The stub status is now handled-or-threw only,
so no observable setter or proxy effect can be replayed by interpreter resume.
The published frame window remains the moving-GC root across the transition;
new typed-array expando materialisation uses the handle arena and re-reads both
the typed array and bag after allocation.

The 2026-07-13 proportional gate passed the JIT/VM/runtime suites, relevant
`clippy -D warnings`, the targeted assignment/accessor/Proxy Test262 subsets,
and all 11 `otter-difftest` corpus cases under `OTTER_GC_STRESS=1..16` with
`OTTER_GC_VERIFY=1`. Cross-chunk and in-process template-corpus tests also
passed at strides 1, 8, and 16. The frozen full Test262 baseline remains
99.02% (51,480/53,173, excluding skipped tests).

An exploratory TypedArray named-expando readback assertion returned
`undefined` (serialized as `null`) under `OTTER_GC_STRESS=1` in the interpreter
oracle, before the tiered half of that assertion ran. The new handle-scoped
materialisation avoids a stale-body write, but that separate pre-existing
TypedArray readback gap is not counted as passing or parity coverage here; the
committed exotic-store matrix uses RegExp `lastIndex` and passes all stress
strides.

Three-run validated V8 v7 medians before/after were Richards `523 -> 531`,
DeltaBlue `277 -> 282`, and Splay `1,173 -> 1,231`. The changes are small
relative to run-to-run variance and are treated as performance-neutral. The
Octane TypeScript sentinel no longer reproduces the historical
`jit_store_prop_stub` abort, but Node, Bun, and Otter all reported `Parse
errors.` and no score marker in this checkout. That run remains failed and
non-scoreable; it does not clear the broader Octane compatibility sentinel.

### Phase 2 production property-load and coercion completion

The production template tier now also completes named `LoadProperty` misses and
coercive `ToPrimitive`/`ToNumeric` operations in place. Property loads retain
their monomorphic own-data fast path; inherited and throwing getters, Proxy
traps, primitive receivers, megamorphic sites, and allocating reentry use the
VM's single full `[[Get]]` implementation. The load transition is total on a
live isolate and returns handled-or-threw, so a getter or proxy effect cannot be
replayed by an exact side exit.

Lowering preserves the bytecode `ToPrimitive` hint. Immediate primitives and
numbers retain their inline paths; observable `@@toPrimitive`, `valueOf`, and
`toString` work enters the shared reentrant VM coercion transition. Per-site
transition blocks are emitted in the function's cold tail so they do not split
hot fast-path blocks. Source, primitive intermediate, and result are held in the
handle arena, and the destination frame register is committed only after
successful completion. The runtime-stub table uses layout version 4 with coercion stub
54. Its production outcomes are handled-or-threw; the bail result exists only
for an isolate-less ABI probe. No raw heap mutation, manual value-root vector,
or derived moving-GC pointer was added.

The last executed Proxy subset failure was an escaped function from
`$262.createRealm()` whose VM-raised error incorrectly used the caller realm's
`TypeError`. Linked bytecode functions now carry a stable scalar realm id, and
error materialisation selects the top frame's realm through the existing traced
realm swap. No realm-local GC handle is copied into function metadata.

The first exact regression run under `OTTER_GC_STRESS=3` exposed a separate
use-after-move during extra-realm `ErrorClassRegistry` finalisation. That run
crashed and is not counted as passing. Realm bootstrap now publishes a
provisional `RealmState` into the interpreter's ordinary traced root graph
before later allocation, re-reads objects after safepoints, and defines each
JS-visible Error global through a handle scope. After the fix, the exact
cross-realm Proxy case passed interpreter-only and template execution at every
stress stride 1 through 16 with `OTTER_GC_VERIFY=1`.

The final targeted Test262 results were assignment `807/816` executed (9 known
failures, 2 skipped), accessors `435/435` executed (13 skipped),
`Symbol.toPrimitive` `3/3` executed (1 skipped), and Proxy `275/275` executed
(36 skipped, zero failures/timeouts/OOM/crashes). Proxy improved by one passing
test. The frozen full-corpus reference remains 99.02% (51,480/53,173, excluding
skipped tests); it was not regenerated from a proportional subset.

The JIT (37 tests), VM (716 tests), and runtime (153 tests, 2 pre-existing
ignored) suites passed, as did relevant all-feature/all-target clippy with
`-D warnings`. Cross-chunk and template-corpus tests passed at stress strides
1, 8, and 16. The release differential corpus passed 11/11 across
`OTTER_GC_STRESS=1..16` with verification enabled.

The first validated build, with coercion slow blocks inline, produced three-run
V8 v7 medians Richards `519`, DeltaBlue `274`, and Splay `1,130`; three more
clean Splay scores were `1,169/1,132/1,178`. Moving those transitions to cold
tails improved the final medians to Richards `522`, DeltaBlue `276`, and Splay
`1,155`. Final raw scores were `526/520/522`, `276/272/278`, and
`1,198/1,155/1,137`; every run reported the suite success marker and is
scoreable. Against the pre-slice medians `531/282/1,231`, the final deltas are
-1.7%, -2.1%, and -6.2% (higher is better). Cold outlining recovered 2.2% on
Splay relative to the inline build, but the remaining Splay loss is retained as
an observed performance regression, not relabelled as noise. The final Octane
TypeScript sentinel reported `ReferenceError: TypeScript is not defined` for
Node, Bun, and Otter and produced no score; it remains failed and non-scoreable.

### Phase 2 production numeric-family and method-error completion

The production template tier now completes the remaining numeric-family fast
misses through one reentrant VM transition. `Sub`, `Mul`, `Div`, `Rem`, the four
relational comparisons, all six binary bitwise/shift operations, `Increment`,
and `Neg` retain their inline Number paths; BigInt, uncommon Number encodings,
and observable update coercion complete through the same VM register helpers as
interpreter dispatch. `Pow` and `BitwiseNot` are now template operations rather
than unsupported side exits. Their cold completion blocks are outlined after
the hot instruction stream. The runtime-stub table remains layout version 4 with numeric
stub 55 and structured-exception stub 56.

The generic `CallMethodValue` transition also owns missing/non-callable errors
after method resolution. Once an accessor or Proxy `[[Get]]` has run, the stub
returns threw instead of exact-bailing and replaying the observable lookup.
Generator, iterator, and pending-bind families still exact-exit before their
bespoke interpreter branches begin.

The live-runtime bailout delta is sixteen numeric opcode miss families removed,
two opcode-level unsupported cases removed (`Pow`, `BitwiseNot`), and two
post-resolution method-error outcomes changed from bail to throw. The supported
template opcode set is 65 of the 172 active bytecodes; argument-count variants
and the other 107 opcodes remain unchanged.

The first targeted build hung in BigInt remainder. This run is invalid and is
not counted as passing: a local Number-remainder slow label shadowed the common
numeric-transition label and looped on the leaf miss. Renaming the local label
and routing its miss to the outlined transition fixed the loop before the
validation gate.

The final gate passed JIT 38/38, VM 716/716, runtime 153/153 unit tests (two
pre-existing ignored) plus all integration tests, and relevant all-target,
all-feature clippy with `-D warnings`. Exponentiation (44/44), bitwise
(106/106), less-than (92/92), less-than-or-equal (47/47), greater-than (92/92),
greater-than-or-equal (43/43), postfix increment (38/38), and prefix increment
(33/33) targeted Test262 subsets had zero failures, skips, timeouts, OOMs, or
crashes. The frozen full-corpus reference remains 99.02% (51,480/53,173,
excluding skipped tests). Cross-chunk and template-corpus tests passed with GC
verification at strides 1, 8, and 16; the property/coercion/numeric/method
transition matrix passed at every stride 1 through 16. The release differential
corpus passed 11/11 at every stride 1 through 16 with verification enabled.

Three captured V8 v7 after scores were Richards `511/525/532`, DeltaBlue
`270/279/280`, Crypto `463/549/546`, and Splay `1,246/1,214/1,410`. The after
medians are `525/279/546/1,246`. Against the preceding documented medians for
Richards `522`, DeltaBlue `276`, and Splay `1,155`, this is approximately
`+0.6%`, `+1.1%`, and `+7.9%` (higher is better). There is no controlled
same-checkpoint Crypto before score, so no speedup is claimed for Crypto. Every
Otter run emitted the suite score marker. Bun failed each combined run with
`ReferenceError: setupEngine is not defined`; those Bun observations are
non-scoreable and do not affect the validated Otter scores.

### Phase 2 production structured-exception completion

The template tier now compiles `EnterTry`, `LeaveTry`, `Throw`, `EndFinally`,
`PopParkedFinally`, and `JumpViaFinally` for whole-function entry. Lowering
consumes the CodeBlock's pre-resolved canonical exception regions. Handler
mutation, throw routing, finally resumption, and abrupt completion walking use
the VM's existing frame/cold-state helpers through reentrant stub 56 (runtime
stub table version 4). A committed effect returns a dynamic same-frame
continuation or a value; it never exact-side-exits and replays the opcode.
Unhandled compiled throws retain the originating frame snapshot and thrown
value across direct-call boundaries.

The active template opcode support delta is six newly compiled exception
opcodes: 65 -> 71 supported active bytecodes, and 107 -> 101 remaining
unsupported opcodes. The previous whole-function rejection for the four core
exception opcodes is removed; `PopParkedFinally` and `JumpViaFinally` no longer
force an OSR-only body. Remaining unsupported families are iterator/generator
ladders, async suspension, spread/eval/class-construction variants, and other
cold bytecodes outside this existing plan.

The targeted exception matrix passed interpreter/template parity, catch and
finally effect counters, abrupt return and loop jumps, direct compiled-callee
throw propagation, and whole-function entry with zero OSR attempts. JIT 38/38,
VM 716/716, runtime 153/153 unit tests (two pre-existing ignored), relevant
clippy `-D warnings`, cross-chunk/template corpus, and the full runtime
integration suite passed. `OTTER_GC_STRESS=1` exception coverage also passed;
the prior 11/11 difftest corpus remains passing at stress strides 1..16 with
GC verification. The frozen Test262 reference remains 99.02% (51,480/53,173,
excluding skipped tests); no full-corpus rerun was required for this compiled
execution-only batch.

Three scoreable V8 v7 runs after the batch produced medians Richards `536`,
DeltaBlue `277`, Crypto `490`, and Splay `1,303`, versus the preceding
documented medians `525`, `279`, `546`, and `1,246`. These are observed
workload deltas of `+2.1%`, `-0.7%`, `-10.3%`, and `+4.6%` respectively (higher
is better); the Crypto regression is retained as measured. Bun again failed
with `ReferenceError: setupEngine is not defined` and is non-scoreable.

### Phase 2 production iterator-lifecycle completion

The template tier now compiles `GetAsyncIterator`, `IteratorNext`,
`IteratorClose`, `IteratorCloseStart`, and `IteratorCloseEnd`. They share
reentrant runtime stub 57 and call the VM's existing complete iterator helpers
rather than copying iterator semantics into JIT code. The transition covers
observable `@@asyncIterator` lookup and async-from-sync fallback, builtin and
user iterators, generators, iterator helpers, result-record accessors, and
`return()` close semantics. An already-active abrupt-close registration is
removed only for the span of a reentrant `next` call and restored only after a
non-done result; a throwing `next` therefore cannot accidentally close or
replay the iterator opcode.

`GetIterator` now completes in machine code as well, through the same reentrant
stub 57. The synchronous `Interpreter::get_iterator_full` helper is the
reentrant sibling of the interpreter's frame-push `drive_get_iterator`: built-in
iterables reuse the shared `run_get_iterator_regs` fast path, while user
`[Symbol.iterator]()` methods, accessor `@@iterator` getters, and TypedArray
prototype iterators run through `run_callable_sync` instead of parking the
opcode on a continuation. The GetIteratorDirect `next` read (once) is factored
into `wrap_iterator_method_result`, now shared by the interpreter resume path
and the compiled transition so both tiers observe identical effects. Every
observable acquisition effect is committed before the destination register is
written; the compiled body no longer side-exits at iterator acquisition. A
whole-function body whose only former blocker was `GetIterator` is now
tier-eligible for entry rather than loop-OSR-only.

The supported active template set is 77 of 172 bytecodes (up from 71); 95
remain unsupported. The abrupt-close registration disarm/re-arm rule for a
reentrant `next` is unchanged: a throwing `next` cannot accidentally close or
replay the iterator opcode.

The focused interpreter/template OSR matrix covers dense arrays and a user
iterator whose `next` and `return` are observable. It passed at normal GC and
with `OTTER_GC_STRESS=1 OTTER_GC_VERIFY=1`. Targeted Test262
`language/statements/for-of` executed 742 tests with 0 failures, timeouts, or
crashes (10 skips). The release differential corpus passed 11/11 under every
GC stress stride 1 through 16 with verification enabled. The frozen full
Test262 reference remains 99.02% (51,480/53,173 excluding skips).

Performance is explicitly subordinate to opcode coverage for this batch: the
goal is a complete template-tier base for the future optimizing tier, not a
short-term score. The iterator family does not touch the Richards/DeltaBlue hot
path; a serial post-batch V8 v7 run of Richards, DeltaBlue, and Splay emitted
suite score markers with zero crash, panic, or missing-marker failure. Earlier
lifecycle-only samples showed a large Richards/Splay swing on an M1-under-load
host with more than 2x run-to-run variance (Splay 571–1,324 in one set); that
variance is treated as host contention, not a code signal, and no before/after
speedup or regression figure is claimed from it. Any real perf recovery is
deferred to the Phase 9 measurement-driven levers once the coverage base and
tier-2 prerequisites are in place.

### Phase 2 production BindFunction completion

The template tier now compiles `Op::BindFunction`. The synchronous
`Interpreter::bind_function_full` is the reentrant sibling of the interpreter's
frame-push `drive_bind_function`: accessor `name`/`length` getters on the bind
target run through `run_callable_sync` instead of parking a
`PendingBindFunction` continuation, so a compiled bind is never resumed after a
partially observed getter. The single VM metadata reader
(`callable_bind_metadata_get`) and the single bound-function allocator
(`finish_bind_function`) are shared with the interpreter, so both tiers observe
identical Proxy/accessor effects and bound-function shape. The intermediate
`name` result is held on the traced iteration-anchor stack across the `length`
getter and the bound allocation; all source operands are re-read from the live
frame registers after every reentrant call. Every observable getter commits
before the bound function is allocated; there is no post-effect side exit. A
bind site with more than four bound arguments still lowers to an exact
pre-effect side exit and serves loop OSR.

The bind path is reentrant stub 58 (`Variadic`, `StatusWord`); `dst`, `callee`,
`this`, and `argc` pack into one machine word and the bound-argument registers
into a second. Template coverage is 78 of 172 active opcodes (up from 77); 94
remain unsupported.

The focused interpreter/template OSR matrix covers plain bound-argument capture
and an accessor `name` getter whose call count is observable; the compiled
counter and every bound result match the interpreter oracle at normal GC and
under `OTTER_GC_STRESS`. Targeted Test262
`built-ins/Function/prototype/bind` executed 97 tests with 0 failures,
timeouts, or crashes (3 skips). The release differential corpus passed 11/11
under GC stress strides 1 through 16 with verification enabled. The frozen full
Test262 reference remains 99.02% (51,480/53,173 excluding skips). Performance
remains subordinate to coverage for this batch.

### Phase 2 production global-access completion

The template tier now compiles the hot global-variable access opcodes:
`LoadGlobalThis`, `LoadGlobalOrUndefined`, `StoreGlobalBinding`, and
`StoreGlobalChecked`. They share reentrant runtime stub 59 and dispatch to the
same `run_*_reg` global environment-record helpers the interpreter uses, so an
accessor-defined global fires identical getters/setters, the declarative-record
lexical shadow and const/TDZ checks are identical, and both tiers observe the
same global-record state. `LoadGlobalOrThrow` was already compiled; this batch
adds the non-throwing read, the `globalThis` read, and both global stores.
Accessor getter/setter reentry runs through `run_callable_sync`; a committed
global effect is never replayed by an exact side exit. The one-time global
*declaration* opcodes (`DeclareGlobalVar`/`Lex`, `DefineGlobalVar`/`Function`,
`InitGlobalLex`, `ValidateGlobalDecl`, `GlobalBindingExists`) remain exact side
exits — they run once at top level, never in a hot loop, so they stay a
separate low-value follow-up rather than part of the hot-access family.

Template coverage is 82 of 172 active opcodes (up from 78); 90 remain
unsupported.

The focused interpreter/template OSR matrix covers a plain global-var
read/write in a hot loop plus an accessor global whose setter call count is
observable; the compiled counter and every global value match the interpreter
oracle at normal GC and under `OTTER_GC_STRESS`. Targeted Test262
`language/expressions/assignment` executed 807/818 (9 pre-existing known
failures, unchanged from the frozen baseline; 0 crashes) and
`language/global-code` passed 195/195. The release differential corpus passed
11/11 under GC stress strides 1 through 16 with verification enabled. The
frozen full Test262 reference remains 99.02% (51,480/53,173 excluding skips).
Performance remains subordinate to coverage for this batch.

### Phase 2 production object property-protocol completion

The template tier now compiles the object property-protocol query family:
`Instanceof`, `HasProperty` (`in`), `GetPrototype`, and `SetPrototype`. They
share reentrant runtime stub 60. Rather than re-implement any protocol
semantics in JIT code, the transition rebuilds the register operands and calls
the interpreter's own Proxy-aware `drive_*_proxy` driver (which fires
`@@hasInstance`, `has`, `getPrototypeOf`, and `setPrototypeOf` traps through
`run_callable_sync`) and otherwise the same `run_*_regs` fast path. A committed
protocol effect is never replayed by an exact side exit.

Completing `HasProperty` from a compiled frame exposed that
`drive_has_property_proxy` hard-required a per-site has-property IC keyed by the
interpreter's pc; a compiled frame has no such site. The IC is a pure fast-path
optimization, so an absent site now skips the IC and falls through to the
`ordinary_has_property_value` spec funnel instead of raising `InvalidOperand`.
Interpreter behavior is unchanged (its pcs always resolve a site).

Template coverage is 86 of 172 active opcodes (up from 82); 86 remain
unsupported.

The focused interpreter/template OSR matrix covers `instanceof`, `in`, and
`getPrototypeOf` over ordinary objects plus a Proxy whose `has`/`getPrototypeOf`
trap call counts are observable; the compiled counters and every result match
the interpreter oracle at normal GC and under `OTTER_GC_STRESS`. Targeted
Test262 `language/expressions/instanceof` (43/43),
`language/expressions/in` (79/79), and `built-ins/Proxy` (275/275, 36 skips)
passed with 0 failures/crashes. The release differential corpus passed 11/11
under GC stress strides 1 through 16 with verification enabled. The frozen full
Test262 reference remains 99.02% (51,480/53,173 excluding skips). Performance
remains subordinate to coverage for this batch.

### Phase 2 production delete completion

The template tier now compiles the `delete` family: `DeleteProperty`,
`DeleteElement`, and `DeleteDynamic`. They share reentrant runtime stub 61. The
transition mirrors interpreter dispatch exactly — the same deferred-namespace
readiness step, the same `drive_delete_*_proxy` driver (Proxy `deleteProperty`
trap through `run_callable_sync`) and `run_delete_*` fast path, and the same
unqualified-delete helper — so no delete semantics are copied into JIT code. A
committed delete, including a strict-mode `Cannot delete property` throw, is
never replayed by an exact side exit.

Template coverage is 89 of 172 active opcodes (up from 86); 83 remain
unsupported.

The focused interpreter/template OSR matrix covers named and computed `delete`
on ordinary objects, a Proxy whose `deleteProperty` trap call count is
observable, and unqualified `delete` of a configurable global; the compiled
counter and every result match the interpreter oracle at normal GC and under
`OTTER_GC_STRESS`. Targeted Test262 `language/expressions/delete` passed 69/69
with 0 failures/crashes. The release differential corpus passed 11/11 under GC
stress strides 1 through 16 with verification enabled. The frozen full Test262
reference remains 99.02% (51,480/53,173 excluding skips). Performance remains
subordinate to coverage for this batch.

### Phase 2 production scalar value-query and coercion completion

The template tier now compiles a wide scalar batch: `ToObject`,
`ToPropertyKey`, `TypeOf`, `LoadNewTarget`, `SameValue`, `IsArray`,
`ArrayLength`, and `LoadLength`, sharing reentrant runtime stub 62. Five
opcodes had their interpreter-dispatch bodies extracted into single-
implementation register helpers (`run_to_object_reg`, `run_to_property_key_reg`,
`run_is_array_reg`, `run_array_length_reg`, `run_load_length_reg`) that both the
interpreter and the compiled transition call; `TypeOf`, `LoadNewTarget`, and
`SameValue` already had such helpers. No scalar semantics are duplicated in JIT
code. `ToPropertyKey` coercion (`@@toPrimitive`/`valueOf`/`toString`) reenters
JS through the shared path; a committed coercion is never replayed by an exact
side exit. `ToNumber` is deliberately excluded from this batch: its interpreter
path is an incomplete foundation that does not re-coerce a `@@toPrimitive`
result, so it will be matched exactly in a separate change rather than diverged.

Template coverage is 97 of 172 active opcodes (up from 89); 75 remain
unsupported.

The focused interpreter/template OSR matrix covers `typeof`, string length,
`Array.isArray`, `Object.is`, and a computed class-field key; the compiled
results match the interpreter oracle at normal GC and under `OTTER_GC_STRESS`.
Targeted Test262 `language/expressions/typeof` (17/17),
`language/statements/class/elements` (1,532/1,534, 2 skips — covers
`ToPropertyKey`), `built-ins/Object/is` (153/153), and `built-ins/Array/isArray`
(29/29) passed with 0 failures/crashes. The release differential corpus passed
11/11 under GC stress strides 1 through 16 with verification enabled. The frozen
full Test262 reference remains 99.02% (51,480/53,173 excluding skips).
Performance remains subordinate to coverage for this batch.

### Phase 2 production dynamic-scope access completion

`LoadDynamic`, `StoreDynamic`, and `TypeofDynamic` now compile through the
existing global environment-record stub (runtime stub 59); they reuse the same
`run_load_dynamic_reg`/`run_store_dynamic_reg`/`run_typeof_dynamic_reg` helpers
the interpreter dispatches, so `with`-scope and sloppy dynamic name resolution
is identical across tiers and no new stub, emitter, or binding was added.
Template coverage is 100 of 172 active opcodes (up from 97); 72 remain
unsupported. Interpreter/template parity holds for a `with`-block fixture;
targeted Test262 `language/statements/with` passed 181/181 and the release
differential corpus passed 11/11 under GC stress strides 1 through 16. The
frozen full Test262 reference remains 99.02% (51,480/53,173 excluding skips).

### Phase 2 production super-access completion

`LoadSuperProperty`, `LoadSuperElement`, `SetSuperProperty`, and
`SetSuperElement` now compile through reentrant runtime stub 63; each calls the
same `run_load_super_property`/`run_store_super_property` helper the interpreter
dispatches, so home-object `[[Prototype]]` accessor getters/setters fire
identically and no super semantics are copied into JIT code. A committed super
effect is never replayed by an exact side exit. Template coverage is 104 of 172
active opcodes (up from 100); 68 remain unsupported. The interpreter/template
OSR matrix covers `super.prop` accessor get/set, `super.method()`, and computed
`super[key]` inside a derived-class method with observable getter/setter
counters; the release differential corpus passed 11/11 under GC stress strides
1 through 16, and targeted Test262 `language/expressions/super` passed 93/94
(1 skip). The frozen full Test262 reference remains 99.02%.

### Phase 2 production private-member completion

`PrivateGet`, `PrivateSet`, and `PrivateBrandCheck` now compile through
reentrant runtime stub 64. Their interpreter-dispatch bodies were extracted into
single-implementation register helpers (`run_private_get_reg`,
`run_private_set_reg`, `run_private_brand_check_reg`) that both the interpreter
and the compiled transition call, so private accessor getters/setters,
brand-check TypeErrors, and non-writable private-method rejections are identical
across tiers and no private-element semantics are duplicated in JIT code. A
committed private effect is never replayed by an exact side exit. Template
coverage is 107 of 172 active opcodes (up from 104); 65 remain unsupported. The
interpreter/template OSR matrix covers private data get/set, private accessor
get/set, a private method call, and a `#field in obj` brand check with
observable getter/setter counters; the release differential corpus passed 11/11
under GC stress strides 1 through 16, and targeted Test262
`language/statements/class/elements/private` passed 187/187. The frozen full
Test262 reference remains 99.02%.

### Phase 2 production static value-load completion

`MathLoad`, `SymbolLoad`, `TemporalLoad`, `LoadBigInt`, and `GetStringIndex`
now compile through reentrant runtime stub 65 (each reuses the interpreter's
`run_math_load_reg`/`run_symbol_load_reg`/`run_temporal_load_reg`/
`run_load_bigint_reg`/`run_get_string_index_regs` helper; the published frame
roots any BigInt-constant or single-code-unit-string allocation). `Nop` lowers
to an empty template no-op. No load semantics are duplicated in JIT code.
Template coverage is 113 of 172 active opcodes (up from 107); 59 remain
unsupported. Interpreter/template parity holds for a `Math.*`/`Symbol.*`/BigInt-
literal/string-index loop; the release differential corpus passed 11/11 under GC
stress strides 1 through 16, and targeted Test262 `built-ins/BigInt` passed
76/77. The frozen full Test262 reference remains 99.02%.

### Phase 2 production allocating-construction completion

`CollectRest`, `NewError`, `NewBuiltinError`, and `ArrayPush` now compile
through reentrant runtime stub 66; each reuses the interpreter's
`run_collect_rest_reg`/`run_new_error_regs`/`run_new_builtin_error_regs`/
`run_array_push_regs` helper, and the published frame roots the array/error
allocation. No construction semantics are duplicated in JIT code. Template
coverage is 117 of 172 active opcodes (up from 113); 55 remain unsupported.
Interpreter/template parity holds for a rest-parameter/array-spread/`new Error`
loop; the release differential corpus passed 11/11 under GC stress strides 1
through 16, and targeted Test262 `built-ins/Error` passed 88/93. The frozen
full Test262 reference remains 99.02%.

### Phase 2 production structural-object completion

`ForInKeys` and `CopyDataProperties` now compile through reentrant runtime stub
67; each rebuilds its register operands and calls the same
`run_for_in_keys_operands`/`run_copy_data_properties_operands` helper the
interpreter dispatches (`CopyDataProperties` reenters any Proxy
`ownKeys`/descriptor trap through the shared path). No structural semantics are
duplicated in JIT code. Template coverage is 119 of 172 active opcodes (up from
117); 53 remain unsupported. Interpreter/template parity holds for a
`for-in`/object-spread loop; the release differential corpus passed 11/11 under
GC stress strides 1 through 16. Targeted Test262 `language/statements/for-in`
was 121/122 — the single failure reproduces identically under interpreter-only
execution, so it is a tier-independent pre-existing baseline failure, not a
regression. The frozen full Test262 reference remains 99.02%.
