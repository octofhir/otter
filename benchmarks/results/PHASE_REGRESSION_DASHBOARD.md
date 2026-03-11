# Phase Regression Dashboard

- Generated (UTC): 2026-03-10T20:17:47Z
- Benchmark: `benchmarks/cpu/flamegraph.ts` (phase mode, scale=1)
- Otter timeout: 45s (`--timeout 45`)
- Otter binary: `/Users/alexanderstreltsov/work/octofhir/otter/target/release/otter`
- Versions: otter=`otter 0.1.2`, node=`v22.16.0`, bun=`1.3.5`, deno=`deno 2.1.1 (stable, release, aarch64-apple-darwin)`

## Results

| Runtime | Phase | Status | Perf flag | Phase ms | Wall ms |
|---|---|---|---|---:|---:|
| otter | math | ok | ok | 92.00 | 115 |
| otter | objects | ok | ok | 491.00 | 511 |
| otter | arrays | ok | ok | 141.00 | 158 |
| otter | strings | ok | ok | 36.00 | 57 |
| otter | calls | ok | ok | 453.00 | 472 |
| otter | json | ok | ok | 6209.00 | 6733 |
| node | math | ok | n/a | 16.35 | 149 |
| node | objects | ok | n/a | 8.93 | 82 |
| node | arrays | ok | n/a | 10.19 | 81 |
| node | strings | ok | n/a | 4.82 | 80 |
| node | calls | ok | n/a | 13.11 | 88 |
| node | json | ok | n/a | 640.66 | 719 |
| bun | math | ok | n/a | 18.59 | 112 |
| bun | objects | ok | n/a | 8.60 | 26 |
| bun | arrays | ok | n/a | 8.18 | 27 |
| bun | strings | ok | n/a | 2.96 | 23 |
| bun | calls | ok | n/a | 12.76 | 30 |
| bun | json | ok | n/a | 563.87 | 584 |
| deno | math | ok | n/a | 40.22 | 89 |
| deno | objects | ok | n/a | 6.91 | 46 |
| deno | arrays | ok | n/a | 10.24 | 50 |
| deno | strings | ok | n/a | 4.06 | 42 |
| deno | calls | ok | n/a | 13.06 | 49 |
| deno | json | ok | n/a | 594.98 | 631 |

Raw data:
- JSON: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260310T201733Z.json`
- TSV: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/phase-baseline-20260310T201733Z.tsv`
