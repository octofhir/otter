# Otter Engine Baseline

- Commit: `48ce1d0d659be6d7c0acfd29b589da0543808df0`
- Outer watchdog: `120000` ms (capture-only; record timeout remains null)
- Platform: `macos` / `aarch64` / `Darwin 25.5.0` / `aarch64`
- Toolchain: `rustc 1.96.0 (ac68faa20 2026-05-25) binary: rustc commit-hash: ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96 commit-date: 2026-05-25 host: aarch64-apple-darwin release: 1.96.0 LLVM version: 22.1.2`

| # | Capture id | Benchmark | JIT | Reuse | Samples | Primary | Unit | Status | Eligible |
| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- |
| 1 | `call-bytecode-a0-interpreter` | `call-bytecode-arity-0` | `interpreter` | `not-applicable` | 20 | 45367917 | `nanoseconds` | `validated` | yes |
| 2 | `call-bytecode-a0-template` | `call-bytecode-arity-0` | `template` | `not-applicable` | 20 | 7706062.5 | `nanoseconds` | `validated` | yes |
| 3 | `call-bytecode-a0-production-tiered` | `call-bytecode-arity-0` | `production-tiered` | `not-applicable` | 20 | 5199292 | `nanoseconds` | `validated` | yes |
| 4 | `call-bytecode-a4-interpreter` | `call-bytecode-arity-4` | `interpreter` | `not-applicable` | 20 | 65218604.5 | `nanoseconds` | `validated` | yes |
| 5 | `call-bytecode-a4-template` | `call-bytecode-arity-4` | `template` | `not-applicable` | 20 | 8161208.5 | `nanoseconds` | `validated` | yes |
| 6 | `call-bytecode-a4-production-tiered` | `call-bytecode-arity-4` | `production-tiered` | `not-applicable` | 20 | 5691021 | `nanoseconds` | `validated` | yes |
| 7 | `call-host-a1-interpreter` | `call-host-arity-1` | `interpreter` | `not-applicable` | 20 | 48270021 | `nanoseconds` | `validated` | yes |
| 8 | `call-host-a1-template` | `call-host-arity-1` | `template` | `not-applicable` | 20 | 7718333.5 | `nanoseconds` | `validated` | yes |
| 9 | `call-host-a1-production-tiered` | `call-host-arity-1` | `production-tiered` | `not-applicable` | 20 | 6828125 | `nanoseconds` | `validated` | yes |
| 10 | `kernel-method-call-monomorphic-interpreter` | `kernel-method-call-monomorphic` | `interpreter` | `not-applicable` | 15 | 647711083 | `nanoseconds` | `validated` | yes |
| 11 | `kernel-method-call-monomorphic-template` | `kernel-method-call-monomorphic` | `template` | `not-applicable` | 15 | 97095291 | `nanoseconds` | `validated` | yes |
| 12 | `kernel-method-call-monomorphic-production-tiered` | `kernel-method-call-monomorphic` | `production-tiered` | `not-applicable` | 15 | 62494042 | `nanoseconds` | `validated` | yes |
| 13 | `kernel-branch-phi-interpreter` | `kernel-branch-phi` | `interpreter` | `not-applicable` | 20 | 628073458.5 | `nanoseconds` | `validated` | yes |
| 14 | `kernel-branch-phi-template` | `kernel-branch-phi` | `template` | `not-applicable` | 20 | 12872916.5 | `nanoseconds` | `validated` | yes |
| 15 | `kernel-branch-phi-production-tiered` | `kernel-branch-phi` | `production-tiered` | `not-applicable` | 20 | 7233666.5 | `nanoseconds` | `validated` | yes |
| 16 | `kernel-boxed-double-property-interpreter` | `kernel-boxed-double-property` | `interpreter` | `not-applicable` | 15 | 531762166 | `nanoseconds` | `validated` | yes |
| 17 | `kernel-boxed-double-property-template` | `kernel-boxed-double-property` | `template` | `not-applicable` | 15 | 15281667 | `nanoseconds` | `validated` | yes |
| 18 | `kernel-boxed-double-property-production-tiered` | `kernel-boxed-double-property` | `production-tiered` | `not-applicable` | 15 | 15333250 | `nanoseconds` | `validated` | yes |
| 19 | `kernel-dense-array-interpreter` | `kernel-dense-array` | `interpreter` | `not-applicable` | 15 | 742800417 | `nanoseconds` | `validated` | yes |
| 20 | `kernel-dense-array-template` | `kernel-dense-array` | `template` | `not-applicable` | 15 | 13017292 | `nanoseconds` | `validated` | yes |
| 21 | `kernel-dense-array-production-tiered` | `kernel-dense-array` | `production-tiered` | `not-applicable` | 15 | 7639916 | `nanoseconds` | `validated` | yes |
| 22 | `kernel-numeric-leaf-interpreter` | `kernel-numeric-leaf` | `interpreter` | `not-applicable` | 20 | 190981646 | `nanoseconds` | `validated` | yes |
| 23 | `kernel-numeric-leaf-template` | `kernel-numeric-leaf` | `template` | `not-applicable` | 20 | 19395854.5 | `nanoseconds` | `validated` | yes |
| 24 | `kernel-numeric-leaf-production-tiered` | `kernel-numeric-leaf` | `production-tiered` | `not-applicable` | 20 | 11138437.5 | `nanoseconds` | `validated` | yes |
| 25 | `jit-compile-engine-target-template` | `jit-compile-engineJitTarget` | `template` | `not-applicable` | 100 | 8041.5 | `nanoseconds` | `validated` | yes |
| 26 | `jit-compile-numeric-leaf-template` | `jit-compile-engineNumericLeaf` | `template` | `not-applicable` | 100 | 11917 | `nanoseconds` | `validated` | yes |
| 27 | `jit-compile-numeric-leaf-optimizing` | `jit-compile-engineNumericLeaf` | `optimizing` | `not-applicable` | 100 | 99625 | `nanoseconds` | `validated` | yes |
| 28 | `memory-forced-full` | `memory-allocation-churn-forced-full` | `interpreter` | `not-applicable` | 5 | 1098552208 | `nanoseconds` | `validated` | yes |
| 29 | `memory-runtime-idle` | `memory-runtime-idle` | `interpreter` | `fresh-per-sample` | 5 | 12539084 | `nanoseconds` | `validated` | yes |
| 30 | `module-module-entry-fresh-per-sample-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 149354 | `nanoseconds` | `validated` | yes |
| 31 | `module-module-entry-fresh-per-sample-template` | `module-phases-module-entry.mjs` | `template` | `fresh-per-sample` | 20 | 167771 | `nanoseconds` | `validated` | yes |
| 32 | `module-module-entry-fresh-per-sample-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `fresh-per-sample` | 20 | 160000 | `nanoseconds` | `validated` | yes |
| 33 | `module-module-entry-reused-across-samples-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `reused-across-samples` | 20 | 130541.5 | `nanoseconds` | `validated` | yes |
| 34 | `module-module-entry-reused-across-samples-template` | `module-phases-module-entry.mjs` | `template` | `reused-across-samples` | 20 | 130917 | `nanoseconds` | `validated` | yes |
| 35 | `module-module-entry-reused-across-samples-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `reused-across-samples` | 20 | 123500 | `nanoseconds` | `validated` | yes |
| 36 | `package-entry-fresh-per-sample-interpreter` | `module-phases-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 177312.5 | `nanoseconds` | `validated` | yes |
