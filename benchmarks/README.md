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
