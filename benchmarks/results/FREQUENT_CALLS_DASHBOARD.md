# Frequent Calls Dashboard

- Generated (UTC): 2026-03-21T17:38:38Z
- Benchmark: `benchmarks/cpu/frequent_calls.ts`
- Scale: `1`
- Otter timeout: 90s
- Versions: otter=`otter 0.1.2`, node=`v22.16.0`, bun=`1.3.5`

| Runtime | Phase | Status | Phase ms | Wall ms |
|---|---|---|---:|---:|
| otter | simple-calls | ok | 21.00 | 43 |
| otter | percent-hex | ok | 7944.00 | 8173 |
| node | simple-calls | ok | 3.32 | 84 |
| node | percent-hex | ok | 55.52 | 127 |
| bun | simple-calls | ok | 3.11 | 25 |
| bun | percent-hex | ok | 98.46 | 117 |

Raw data:
- JSON: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/frequent-calls-baseline-20260321T173829Z.json`
- TSV: `/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/frequent-calls-baseline-20260321T173829Z.tsv`
