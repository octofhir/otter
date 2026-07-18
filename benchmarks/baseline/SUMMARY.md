# Otter Engine Baseline

- Commit: `56aa401611db3ea0cb88c35c94070080c57d9318`
- Outer watchdog: `120000` ms (capture-only; record timeout remains null)
- Platform: `macos` / `aarch64` / `Darwin 25.5.0` / `aarch64`
- Toolchain: `rustc 1.96.0 (ac68faa20 2026-05-25) binary: rustc commit-hash: ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96 commit-date: 2026-05-25 host: aarch64-apple-darwin release: 1.96.0 LLVM version: 22.1.2`

| # | Capture id | Benchmark | JIT | Reuse | Samples | Primary | Unit | Status | Eligible |
| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- |
| 1 | `call-bytecode-a0-interpreter` | `call-bytecode-arity-0` | `interpreter` | `not-applicable` | 20 | 45622229 | `nanoseconds` | `validated` | yes |
| 2 | `call-bytecode-a0-template` | `call-bytecode-arity-0` | `template` | `not-applicable` | 20 | 7588375 | `nanoseconds` | `validated` | yes |
| 3 | `call-bytecode-a0-production-tiered` | `call-bytecode-arity-0` | `production-tiered` | `not-applicable` | 20 | 7053708 | `nanoseconds` | `validated` | yes |
| 4 | `call-bytecode-a4-interpreter` | `call-bytecode-arity-4` | `interpreter` | `not-applicable` | 20 | 64513354 | `nanoseconds` | `validated` | yes |
| 5 | `call-bytecode-a4-template` | `call-bytecode-arity-4` | `template` | `not-applicable` | 20 | 7952375 | `nanoseconds` | `validated` | yes |
| 6 | `call-bytecode-a4-production-tiered` | `call-bytecode-arity-4` | `production-tiered` | `not-applicable` | 20 | 7524729 | `nanoseconds` | `validated` | yes |
| 7 | `call-host-a1-interpreter` | `call-host-arity-1` | `interpreter` | `not-applicable` | 20 | 52943083 | `nanoseconds` | `validated` | yes |
| 8 | `call-host-a1-template` | `call-host-arity-1` | `template` | `not-applicable` | 20 | 8334020.5 | `nanoseconds` | `validated` | yes |
| 9 | `call-host-a1-production-tiered` | `call-host-arity-1` | `production-tiered` | `not-applicable` | 20 | 49804687.5 | `nanoseconds` | `validated` | yes |
| 10 | `jit-compile-engine-target` | `jit-compile-engineJitTarget` | `template` | `not-applicable` | 100 | 12479.5 | `nanoseconds` | `validated` | yes |
| 11 | `memory-forced-full` | `memory-allocation-churn-forced-full` | `interpreter` | `not-applicable` | 5 | 1102310792 | `nanoseconds` | `validated` | yes |
| 12 | `module-module-entry-fresh-per-sample-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 168416.5 | `nanoseconds` | `validated` | yes |
| 13 | `module-module-entry-fresh-per-sample-template` | `module-phases-module-entry.mjs` | `template` | `fresh-per-sample` | 20 | 162270.5 | `nanoseconds` | `validated` | yes |
| 14 | `module-module-entry-fresh-per-sample-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `fresh-per-sample` | 20 | 157167 | `nanoseconds` | `validated` | yes |
| 15 | `module-module-entry-reused-across-samples-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `reused-across-samples` | 20 | 127312.5 | `nanoseconds` | `validated` | yes |
| 16 | `module-module-entry-reused-across-samples-template` | `module-phases-module-entry.mjs` | `template` | `reused-across-samples` | 20 | 126812 | `nanoseconds` | `validated` | yes |
| 17 | `module-module-entry-reused-across-samples-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `reused-across-samples` | 20 | 129374.5 | `nanoseconds` | `validated` | yes |
| 18 | `package-entry-fresh-per-sample-interpreter` | `module-phases-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 186375 | `nanoseconds` | `validated` | yes |
