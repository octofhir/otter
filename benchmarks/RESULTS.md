# Benchmark Results

Generated: 2026-07-09 on macOS arm64. Benchmark runners use
`OTTER_BENCH_TIMEOUT=0` by default, so Otter runs are not wall-clock capped.

Raw logs are under `benchmarks/results/` and are intentionally ignored by git.

## Summary

| Suite | Workload | Node | Bun | Otter | Log |
| --- | --- | ---: | ---: | ---: | --- |
| V8 v7 | full suite score, higher is better | 51135 | 23490 | 239 | `benchmarks/results/v8-v7-20260709T182435Z.log` |
| ARES-6 | full suite summary, lower ms is better | 8.00 ms | 8.20 ms | fail | `benchmarks/results/ares6-20260709T184236Z.log` |
| Web Tooling | `babel`, higher runs/s is better | 21.40 runs/s | fail | fail | `benchmarks/results/web-tooling-20260709T185458Z.log` |
| yt-dlp/ejs | 1 iteration, lower ms is better | 13933 ms | 10941 ms | fail | `benchmarks/results/ejs-20260709T184637Z.log` |

V8 v7 composite puts Otter at about 214x slower than Node and 98x slower than
Bun on this machine.

## V8 v7

| Workload | Node | Bun | Otter |
| --- | ---: | ---: | ---: |
| Richards | 42115 | 29537 | 191 |
| DeltaBlue | 99051 | 31849 | 146 |
| Crypto | 56505 | 20314 | 339 |
| RayTrace | 89687 | 51577 | 177 |
| EarleyBoyer | 94549 | 34539 | 537 |
| RegExp | 14015 | 9038 | 225 |
| Splay | 45024 | 15891 | 560 |
| NavierStokes | 37063 | 18957 | 95.1 |
| Score | 51135 | 23490 | 239 |

## Octane

Octane is run per workload so one compile/runtime failure does not hide the rest
of the suite. Some old Octane workloads also fail under modern Node/Bun in this
shell-style harness; those are recorded as failures instead of being hidden.

| Workload | Node | Bun | Otter |
| --- | ---: | ---: | ---: |
| richards | 35263 | 48343 | 531 |
| deltablue | 130008 | 75593 | 267 |
| crypto | 57971 | fail: `setupEngine` | 967 |
| raytrace | 89243 | 196541 | fail: `SIGSEGV` |
| earley-boyer | 95631 | 102730 | 610 |
| regexp | 13157 | 18729 | 148 |
| splay | 8142 | 16044 | fail: `SplayLatency: NaN` |
| navier-stokes | 12834 | fail: `TypeError` | 86.4 |
| pdfjs | fail: `PDFJS.getPdf` | fail: `PDFJS.getPdf` | fail: `TypeError` |
| mandreel | 77507 | 88036 | fail: 65535-register window |
| gbemu | 128300 | fail: readonly property | fail: incorrect sample length |
| code-load | fail: `MockElement` | fail: incorrect result | fail: invalid operand |
| box2d | 137855 | 150488 | fail: dynasm relocation panic |
| zlib | fail: `print` | fail: strict-mode syntax | fail: `print` |
| typescript | fail: parse errors | fail: parse errors | fail: `TypeError` |

Log: `benchmarks/results/octane-20260709T183513Z.log`.

## Failure Notes

- ARES-6: Otter fails in Air with `Wrong early hash for createPayloadGbemuExecuteIteration` followed by `NOT_CALLABLE`.
- Web Tooling `babel`: Otter fails compiling the 32.7 MB bundle because a call site has 528 arguments and exceeds the current 240-argument limit.
- yt-dlp/ejs: Otter fails before execution because `import { type ESTree } from "meriyah"` is treated as a runtime import and `ESTree` is not exported by `meriyah`.
