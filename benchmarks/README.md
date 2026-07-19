# Benchmarks

Otter has two deliberately separate benchmark surfaces:

- `otter-engine-benchmark` provides controlled VM/runtime/JIT measurements for
  engine stabilization and optimization work.
- The checked-in suite runners exercise recognized JavaScript workloads after
  an engine change clears its focused gates.

The current engine baseline does not install packages or bootstrap Node/Web API
surfaces. ARES-6, Web Tooling, and yt-dlp/ejs remain useful compatibility and
macro-performance suites, but they are outside the engine-only baseline.

## Current result contract

Every engine benchmark command writes one machine-readable JSON record. There
is one live format: changes are intentionally breaking and land together with
the runner, fixtures, tests, and this documentation. There is no compatibility
reader or legacy output mode.

Primary and secondary metric sample arrays match `sampling.sampleCount`.
Run-level diagnostics instead use one raw value with the `single` statistic;
they are never ranking inputs and need not fabricate one copy per timed sample.

A record is scoreable only when the workload completes successfully, its
semantic result is validated, and it contains a primary metric. Baseline
eligibility is stricter: provenance must report a clean worktree and the
capture must use a release build. A dirty but otherwise valid observation can
remain scoreable for local investigation, but it is never baseline-eligible.
Failed, timed-out, unavailable, and unvalidated observations remain explicit
and non-scoreable. `timeoutMs` stays null unless that exact timeout was enforced
by the process producing the record.

The former Phase 0 evidence and dashboard are historical artifacts, not inputs
to the current baseline. A current baseline is published only through the
clean-commit capture workflow below.

## Engine benchmark binary

Build or run the focused harness with the `engine` feature:

```bash
cargo build --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark
```

Tier choice is explicit whenever a workload permits tier selection:

- `interpreter` runs without native compilation.
- `template` uses the production template compiler only.
- `production-tiered` uses the current production tier policy.

`call`, `kernel`, and `module` require `--jit-tier`; their optional
`--jit-osr-threshold` records a deliberate threshold override. `jit-compile`
requires `--compile-tier template|optimizing` and one or more explicit numeric
`--argument` values. `memory` always measures the interpreter with a post-run
full GC. `idle-memory` aggregates fresh-process interpreter runtimes at a
forced-full-GC idle boundary. Do not infer a benchmark tier from legacy JIT
environment variables.

### JavaScript kernel corpus

Engine-only JavaScript kernels live in `benchmarks/scripts/`. Each file is a
plain script that declares one global, zero-argument `engineKernel` function.
Loading and compiling the file, compiling the invocation stub, and evaluating
top-level fixture setup happen outside measured samples. A measured sample
executes the precompiled global lookup and `engineKernel()` call, and accepts
only the exact finite `Number` listed below. Kernels must not read clocks,
print output, access the filesystem, install packages, or depend on host APIs.

| Fixture | Engine signal | Exact result |
| --- | --- | ---: |
| `method-call-monomorphic.js` | Stable receiver shape and monomorphic method dispatch | `500003500000` |
| `branch-phi.js` | Alternating control flow and numeric value merge | `-6000000` |
| `boxed-double-property.js` | Repeated double-valued object property loads | `4000000` |
| `dense-array.js` | Repeated dense indexed array loads | `5234688` |
| `numeric-leaf.js` | Repeated calls into a straight-line eight-operation Number leaf | `-700000` |

Run one fixture per isolate. Warmups and measured samples reuse that isolate
and the same precompiled invocation stub, but every invocation must return the
same exact result. The fixtures contain no timers or validation output; the
harness owns timing, tier selection, sampling, and checksum validation.

Each kernel record also emits the complete `JitRuntimeStats` delta from after
fixture setup through the last measured sample as informational `jit-*`
diagnostic counters. The before/after snapshots are outside timed samples,
include warmup tiering, and retain explicit zeroes so interpreter runs prove
that no native work occurred. Generated template and optimizing entries,
returns, throws, and cold deopts remain separate from VM-entered optimizer
activity.

```bash
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  kernel --source benchmarks/scripts/method-call-monomorphic.js \
  --function engineKernel --expected 500003500000 \
  --jit-tier production-tiered --samples 20 --warmup 3
```

### Call execution

The call workload is parsed and lowered before sampling. Every warmup and
measured execution validates the returned sum.

```bash
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  call --kind bytecode --arity 4 --jit-tier production-tiered \
  --iterations 100000 --samples 20 --warmup 3
```

Direct calls wider than the active bytecode format must be recorded as
non-scoreable failures. Do not rewrite them into a spread-call workload.

### JIT compilation

The compile fixture calls the target function directly with the declared
numeric arguments, seeds feedback, then retains the final measured machine-code
artifact. It installs that exact object through an isolate-local validation
hook, enters it with the same direct arguments, and checks the result before
emitter samples are accepted. Source parsing, bytecode lowering, feedback
seeding, snapshot construction, backend-marker preflight, and compiler-hook
construction remain outside the timer; emitter work and executable-buffer
finalization remain inside it.

```bash
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  jit-compile --source benchmarks/fixtures/engine/jit-compile.js \
  --function engineJitTarget --expected 33 --compile-tier template \
  --argument 1 --argument 2 \
  --samples 100 --warmup 10
```

The optimizing numeric-leaf row additionally passes
`--expect-backend cranelift-numeric-leaf`. The command captures one untimed
artifact preflight and requires `optimized-ir.txt` to begin with
`; backend=cranelift numeric-leaf`; a silent fallback to the general optimizer
is a validation failure.

### Managed memory

The memory workload uses fresh interpreter isolates. Its execution timer covers
JavaScript execution; wall time also includes one post-run forced full GC.
Allocation and cumulative pause deltas exclude bootstrap, and retained heap is
sampled only after full reconciliation.

```bash
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  memory --iterations 1000000 --samples 5
```

### Idle runtime memory

`idle-memory` launches every raw sample as a separate copy of the current
release benchmark executable. Each child constructs one interpreter-only
`Runtime`, executes an empty script turn (whose normal runtime boundary drains
microtasks), proves that no timer callbacks or host-settlement promises remain,
forces a full GC, and waits through a controlled idle window.

The parent measures startup from process spawn until the child publishes its
post-bootstrap marker. The child records current process RSS after bootstrap,
after full GC, and at the end of the idle window; it also records full-GC
cycles and pause time, retained/live heap, allocated managed-heap bytes,
managed page capacity, off-heap reservations, and the exact measured
executable size. Metrics are emitted only when the host exposes a real RSS
reading. The sysinfo process table is initialized before runtime construction,
so its benchmark-only bookkeeping remains stable across all three RSS phases;
ordinary runtime construction does not enable a sampler, lock, or diagnostic
heap walk.

```bash
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  idle-memory --idle-ms 250 --samples 5
```

### Module graph

`--runtime-reuse fresh-per-sample` gives every measured graph execution a fresh
runtime. `--runtime-reuse reused-across-samples` validates one runtime during
warmup and then reuses it for measured executions. In both modes the graph is
resolved, loaded, parsed, compiled, linked, and executed again; runtime reuse is
not a module cache hit. Fresh-per-sample warmups also construct, validate, and
discard a new runtime each time; they stabilize process/CPU state without
warming a runtime or module cache used by measured samples.

```bash
# Fresh runtime per sample.
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  module --entry benchmarks/fixtures/engine/module-entry.mjs \
  --jit-tier production-tiered --runtime-reuse fresh-per-sample \
  --samples 20 --warmup 5

# One persistent runtime after validated warmup.
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  module --entry benchmarks/fixtures/engine/module-entry.mjs \
  --jit-tier production-tiered --runtime-reuse reused-across-samples \
  --samples 20 --warmup 5

# Package-import-map resolution without a generated node_modules tree.
cargo run --release -p otter-benchmark --features engine \
  --bin otter-engine-benchmark -- \
  module --entry benchmarks/fixtures/engine/package/entry.mjs \
  --jit-tier production-tiered --runtime-reuse fresh-per-sample \
  --samples 20 --warmup 5
```

The package fixture resolves `#engine-dep` through its checked-in
`package.json#imports` map.

## External command recorder

`otter-benchmark` wraps one child process in the same live result contract. It
records the complete recorder invocation, a real child exit code when one
exists, and the exact enforced timeout. The default path uses
`Command::output`; timeout polling and RSS sampling stay opt-in.

```bash
cargo run --release -p otter-benchmark --features rss -- \
  --suite comparison --name otter-richards \
  --surface cli-process --jit-policy production-tiered \
  --validation-marker 'Richards:' \
  --build-profile release --timeout-ms 45000 --rss-sample-ms 100 -- \
  target/release/otter run benchmarks/scripts/richards.js
```

`--build-profile` is an explicit assertion about the measured executable, not
the recorder itself. Omit it for external binaries whose build profile cannot
be verified; the record then uses `unknown` and cannot enter a checked-in
baseline. When RSS is enabled, the requested cadence is preserved in the
reserved `recorder.rss-sample-ms` benchmark parameter. A timeout also bounds
pipe collection, so descendants retaining inherited output descriptors cannot
keep the recorder alive past the declared wall cap.

## Capturing a baseline

The baseline driver owns one fixed, ordered 36-case engine matrix: bytecode
calls at arity 0 and 4 across all tiers, the extracted host call across all
tiers, five JavaScript kernels across all tiers, one existing template compile
row, template and optimizing numeric-leaf compile rows, forced-full-GC
allocation churn, fresh-process idle runtime memory, fresh and reused module
runtimes across all tiers, and isolated package-import resolution. It runs
serially and does not install packages, enable Web/Node surfaces, or start a
profiler.

Capture from a clean commit with the release driver:

```bash
cargo run --locked --release -p otter-benchmark --features engine \
  --bin otter-engine-baseline -- capture
```

`capture` builds the release engine binary with `--locked`, rechecks the same
clean HEAD, and writes raw stdout/stderr, exact benchmark records, an
unversioned manifest, and a derived summary below the ignored
`benchmarks/results/` directory. Every child therefore continues to report
`dirty: false`. The default 120-second outer watchdog exists only in
`capture.json`; child records keep `sampling.timeoutMs: null` because the
engine process itself did not enforce that timeout. An outer timeout preserves
raw evidence and prevents publication; it never fabricates a benchmark
record.

The driver rejects `OTTER_JIT*`, `OTTER_GC*`, and `RUST_LOG` overrides so the
recorded configuration remains the complete performance policy. A capture is
publishable only when all 36 exact commands returned zero, every result passes
the live contract, every result is clean/release/baseline-eligible, and commit,
platform, toolchain, argv, configuration, and sampling protocol remain
identical to the fixed matrix.

Publish the successful ignored capture explicitly:

```bash
cargo run --locked --release -p otter-benchmark --features engine \
  --bin otter-engine-baseline -- publish \
  --capture benchmarks/results/engine-<commit>-<timestamp>
```

`publish` revalidates every byte and regenerates the summary before installing
the one current `benchmarks/baseline/` directory. An existing current baseline
is moved to ignored staging and restored if installation fails; a successful
publication removes it. The command refuses an incomplete capture,
changed/dirty HEAD, edited summary, non-scoreable row, legacy tier, or
mismatched provenance. There is no format generation, compatibility reader, or
fallback to archived data.

Raw logs and local captures belong under `benchmarks/results/`, which is
ignored by git. A curated baseline must retain non-scoreable observations
rather than silently dropping them, and its Markdown summary must be derived
from the same machine-readable records.

The active opcode effect and tier-support inventory is available separately:

```bash
cargo run -p otter-bytecode --bin opcode-audit
```

## External suites

| Suite | Runner | Signal |
| --- | --- | --- |
| V8 v7 | `benchmarks/run-v8-v7.sh` | Classic language/runtime throughput; higher is better. |
| Octane | `benchmarks/run-octane.sh` | Larger historical V8 workloads; higher is better. |
| ARES-6 | `benchmarks/run-ares6.sh` | Compiler/parser/runtime workloads; lower ms is better. |
| Web Tooling Benchmark | `benchmarks/run-web-tooling.sh` | Babel/Terser/Acorn bundles; lower ms is better. |
| yt-dlp/ejs | `benchmarks/run-ejs.sh` | TypeScript/ESM parser-transform workload; lower ms is better. |

Downloaded checkouts and generated bundles live under
`benchmarks/.suite-cache/` and are ignored by git.

```bash
# Fast suite smoke after focused engine gates.
just bench

# Full or selected suites.
benchmarks/run-v8-v7.sh
benchmarks/run-v8-v7.sh richards regexp
benchmarks/run-octane.sh
benchmarks/run-octane.sh richards crypto splay
benchmarks/run-ares6.sh air basic
benchmarks/run-web-tooling.sh --only babel
benchmarks/run-ejs.sh
```

V8 v7 and Octane use shell-style multi-file loading in one runtime/realm:

```bash
otter run base.js richards.js run-driver.js
```

## Historical artifacts

These files preserve evidence at their captured commits. Their paths, commands,
tier names, and data formats are unsupported and must not be treated as the
current benchmark contract:

- [`archive/PHASE0_BASELINE.md`](archive/PHASE0_BASELINE.md)
- [`archive/PERF_DASHBOARD-2026-07-16-69988580.json`](archive/PERF_DASHBOARD-2026-07-16-69988580.json)
- [`archive/RESULTS-2026-07-09.md`](archive/RESULTS-2026-07-09.md)
