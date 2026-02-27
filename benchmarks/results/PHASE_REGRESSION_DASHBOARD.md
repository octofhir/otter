# Phase Regression Dashboard

- Generated (UTC): 2026-02-27T18:58:06Z
- Benchmark: `benchmarks/cpu/flamegraph.ts` (phase mode, scale=1)
- Otter timeout: 45s (`--timeout 45`)
- Otter binary: `/Users/alexanderstreltsov/work/octofhir/otter/target/release/otter`
- Versions: otter=`otter 0.1.2`, node=`v22.16.0`, bun=`1.3.5`, deno=`deno 2.1.1 (stable, release, aarch64-apple-darwin)`

## Results

| Runtime | Phase | Status | Perf flag | Phase ms | Wall ms |
|---|---|---|---|---:|---:|
| otter | math | timeout | critical-timeout | n/a | 45029 |
| otter | objects | timeout | critical-timeout | n/a | 45035 |
| otter | arrays | timeout | critical-timeout | n/a | 45071 |
| otter | strings | ok | ok | 11458.00 | 11546 |
| otter | calls | timeout | critical-timeout | n/a | 45098 |
| otter | json | ok | ok | 19870.00 | 21871 |
| node | math | ok | n/a | 18.08 | 206 |
| node | objects | ok | n/a | 10.04 | 92 |
| node | arrays | ok | n/a | 13.17 | 95 |
| node | strings | ok | n/a | 4.61 | 90 |
| node | calls | ok | n/a | 15.53 | 100 |
| node | json | ok | n/a | 722.70 | 809 |
| bun | math | ok | n/a | 21.17 | 134 |
| bun | objects | ok | n/a | 9.14 | 33 |
| bun | arrays | ok | n/a | 9.84 | 31 |
| bun | strings | ok | n/a | 3.34 | 27 |
| bun | calls | ok | n/a | 14.72 | 36 |
| bun | json | ok | n/a | 958.70 | 979 |
| deno | math | ok | n/a | 49.59 | 200 |
| deno | objects | ok | n/a | 7.77 | 38 |
| deno | arrays | ok | n/a | 10.84 | 43 |
| deno | strings | ok | n/a | 3.92 | 35 |
| deno | calls | ok | n/a | 12.75 | 44 |
| deno | json | ok | n/a | 624.56 | 657 |

Raw data:
- JSON: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260227T185427Z.json`
- TSV: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260227T185427Z.tsv`
