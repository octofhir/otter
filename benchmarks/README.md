# Benchmarks

Otter uses standard JavaScript benchmark suites here, not local microbenchmarks.
The goal is to measure workloads other JS engines and papers already recognize:
V8 v7, Octane, ARES-6, V8 Web Tooling Benchmark, and yt-dlp/ejs.

The old `benchmarks/scripts/*.js` microbench set was removed because it mostly
measured hand-picked hot loops and process startup. Those numbers were useful
while debugging early VM bugs, but they are a poor performance target.

## Suites

| Suite | Runner | Signal |
| --- | --- | --- |
| V8 v7 | `benchmarks/run-v8-v7.sh` | Classic language/runtime throughput; higher is better. |
| Octane | `benchmarks/run-octane.sh` | Larger historical V8 workloads; higher is better. |
| ARES-6 | `benchmarks/run-ares6.sh` | Modern compiler/parser/runtime workloads; lower ms is better. |
| Web Tooling Benchmark | `benchmarks/run-web-tooling.sh` | Real JS tooling bundles such as Babel/Terser/Acorn; lower ms is better. |
| yt-dlp/ejs | `benchmarks/run-ejs.sh` | Real TypeScript/ESM parser-transform workload over cached YouTube player JS fixtures; lower ms is better. |

Downloaded checkouts and generated bundles live under
`benchmarks/.suite-cache/` and are ignored by git. Raw logs are written to
`benchmarks/results/` and are also ignored. Curated current results belong in
[`RESULTS.md`](RESULTS.md).

Phase 0 VM/JIT refactor evidence, correctness blockers, and reproduction
commands are tracked in [`PHASE0_BASELINE.md`](PHASE0_BASELINE.md).

## Machine-readable result envelope

Use `otter-benchmark` to wrap new benchmark commands. Its versioned JSON record
always includes environment/mode/cache/correctness fields and explicit `null`
values for counters that the wrapped command cannot yet provide. A missing
validation marker makes the result non-scoreable.

```bash
cargo run -p otter-benchmark -- \
  --name smoke --runtime-mode cli --jit-mode baseline \
  --gc-mode normal --cache-state cold --validation-marker ok -- \
  target/release/otter -p '"ok"'
```

The active opcode-effect/support inventory is emitted with:

```bash
cargo run -p otter-bytecode --bin opcode-audit
```

Focused Phase 0 call and baseline-emitter measurements use `otter-phase0`.
Both subcommands emit the same schema-v1 JSON envelope and refuse to score a
sample whose expected result was not observed:

```bash
cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  call --kind bytecode --arity 4 --jit-mode baseline \
  --iterations 100000 --samples 20 --warmup 3

cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  jit-compile --source benchmarks/fixtures/phase0/jit-compile.js \
  --function phase0JitTarget --expected 3300 --samples 100 --warmup 10

cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  memory --iterations 1000000 --samples 5

cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  module --entry benchmarks/fixtures/phase0/module-entry.mjs \
  --cache-state cold --samples 20 --warmup 0

cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  module --entry benchmarks/fixtures/phase0/module-entry.mjs \
  --cache-state warm --samples 20 --warmup 5

cargo run --release -p otter-benchmark --features phase0 --bin otter-phase0 -- \
  module --entry benchmarks/fixtures/phase0/package/entry.mjs \
  --cache-state cold --samples 20 --warmup 0

cargo run --release -p otter-benchmark --features phase0,rss --bin otter-phase0 -- \
  macro-memory --name v8-v7-richards-memory \
  --source benchmarks/.suite-cache/v8-v7/base.js \
           benchmarks/.suite-cache/v8-v7/richards.js \
           benchmarks/.suite-cache/v8-v7/driver.js \
  --validation-marker 'Score (version 7):' --samples 5 --rss-sample-ms 5
```

Direct calls wider than the current bytecode format should be recorded as
failures, not rewritten into a different spread-call workload and not assigned
a performance score.

The `memory` workload uses fresh interpreter isolates. Its execution timer
covers JS only; wall time additionally includes one post-run forced full GC.
Allocation and cumulative pause deltas exclude bootstrap, and `heap_bytes` is
sampled after that full reconciliation so it represents retained live heap
rather than allocation-accounting bytes awaiting collection.

For the `module` workload, `cold` means a fresh `Runtime` for every measured
graph execution. `warm` means one persistent `Runtime`, five validated
pre-executions by default, then repeated measured executions. Both modes still
resolve, read, parse, compile, and link a fresh graph: Otter has no persistent
CodeBlock or module cache yet. The warm label therefore describes runtime and
host filesystem state, not a module-cache hit.

The package-backed fixture resolves `#phase0-dep` through its checked-in
`package.json#imports` map, so the same `module` command also captures package
scope/manifest resolution without a generated `node_modules` tree.

`macro-memory` preserves the CLI multi-file script semantics: every input is
compiled as a classic script in one CLI-equivalent runtime, in source order.
Each sample must emit the requested suite marker, then the runner forces a full
GC and records retained managed heap, cumulative workload/GC pause time, and
the deduplicated finalized bytes reachable from all JIT entry/OSR/direct-call
caches. RSS polling is opt-in and runs on a separate sampler thread only when
`--rss-sample-ms` is non-zero. Prepare the V8 v7 cache/driver with
`benchmarks/run-v8-v7.sh richards` before invoking the command directly.

Peak RSS sampling is opt-in on the generic recorder and therefore adds no
polling overhead to ordinary runs:

```bash
cargo run --release -p otter-benchmark --features rss -- \
  --name memory-rss --runtime-mode vm --jit-mode interpreter-only \
  --gc-mode forced-full --rss-sample-ms 5 \
  --validation-marker 'return=500000500000' -- \
  target/release/otter-phase0 memory --iterations 1000000 --samples 5
```

## Run

```bash
# Fast baseline: V8 v7 + a small Octane smoke selection.
just bench

# Full individual suites.
benchmarks/run-v8-v7.sh
benchmarks/run-octane.sh
benchmarks/run-ares6.sh
benchmarks/run-web-tooling.sh --only babel
benchmarks/run-ejs.sh

# Selected workloads.
benchmarks/run-v8-v7.sh richards regexp
benchmarks/run-octane.sh richards crypto splay
benchmarks/run-ares6.sh air basic
```

All runners build `target/release/otter` unless `OTTER_BIN=/path/to/otter` is
set. `OTTER_JIT=1` is the default for benchmark runs.

V8 v7 and Octane are run through Otter's shell-style multi-file CLI loading:

```bash
otter run base.js richards.js run-driver.js
```

That mode executes each file as a global script in one runtime/realm, matching
the shell model used by these suites.

## Updating Results

Run the suites you want to publish, then summarize the generated logs into
`benchmarks/RESULTS.md`. Do not commit raw `benchmarks/results/*.log` files.
