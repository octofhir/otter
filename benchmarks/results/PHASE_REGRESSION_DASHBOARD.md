# Phase Regression Dashboard

- Generated (UTC): 2026-02-26T23:10:07Z
- Benchmark: `benchmarks/cpu/flamegraph.ts` (phase mode, scale=1)
- Otter timeout: 45s (`--timeout 45`)
- Otter binary: `/Users/alexanderstreltsov/work/octofhir/otter/target/release/otter`
- Versions: otter=`otter 0.1.2`, node=`v22.16.0`, bun=`1.3.5`, deno=`deno 2.1.1 (stable, release, aarch64-apple-darwin)`

## Results

| Runtime | Phase | Status | Perf flag | Phase ms | Wall ms |
|---|---|---|---|---:|---:|
| otter | math | timeout | critical-timeout | n/a | 45064 |
| otter | objects | timeout | critical-timeout | n/a | 45103 |
| otter | arrays | timeout | critical-timeout | n/a | 45178 |
| otter | strings | ok | ok | 19068.00 | 19295 |
| otter | calls | timeout | critical-timeout | n/a | 45161 |
| otter | json | timeout | critical-timeout | n/a | 52269 |
| node | math | ok | n/a | 82.88 | 446 |
| node | objects | ok | n/a | 76.77 | 358 |
| node | arrays | ok | n/a | 20.04 | 424 |
| node | strings | ok | n/a | 98.25 | 354 |
| node | calls | ok | n/a | 76.60 | 296 |
| node | json | ok | n/a | 1983.31 | 2322 |
| bun | math | ok | n/a | 25.62 | 141 |
| bun | objects | ok | n/a | 38.73 | 168 |
| bun | arrays | ok | n/a | 44.23 | 165 |
| bun | strings | ok | n/a | 23.17 | 113 |
| bun | calls | ok | n/a | 50.19 | 205 |
| bun | json | ok | n/a | 2725.36 | 2855 |
| deno | math | ok | n/a | 75.21 | 353 |
| deno | objects | ok | n/a | 42.08 | 263 |
| deno | arrays | ok | n/a | 44.79 | 258 |
| deno | strings | ok | n/a | 9.44 | 94 |
| deno | calls | ok | n/a | 44.59 | 202 |
| deno | json | ok | n/a | 2148.79 | 2290 |

Raw data:
- JSON: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260226T230541Z.json`
- TSV: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260226T230541Z.tsv`
