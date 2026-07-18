# Otter Engine Baseline

- Commit: `bdb28d84c6ae5004bf8c82e94da554097b1922e6`
- Outer watchdog: `120000` ms (capture-only; record timeout remains null)
- Platform: `macos` / `aarch64` / `Darwin 25.5.0` / `aarch64`
- Toolchain: `rustc 1.96.0 (ac68faa20 2026-05-25) binary: rustc commit-hash: ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96 commit-date: 2026-05-25 host: aarch64-apple-darwin release: 1.96.0 LLVM version: 22.1.2`

| # | Capture id | Benchmark | JIT | Reuse | Samples | Primary | Unit | Status | Eligible |
| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- |
| 1 | `call-bytecode-a0-interpreter` | `call-bytecode-arity-0` | `interpreter` | `not-applicable` | 20 | 46598646 | `nanoseconds` | `validated` | yes |
| 2 | `call-bytecode-a0-template` | `call-bytecode-arity-0` | `template` | `not-applicable` | 20 | 7548500 | `nanoseconds` | `validated` | yes |
| 3 | `call-bytecode-a0-production-tiered` | `call-bytecode-arity-0` | `production-tiered` | `not-applicable` | 20 | 6922125 | `nanoseconds` | `validated` | yes |
| 4 | `call-bytecode-a4-interpreter` | `call-bytecode-arity-4` | `interpreter` | `not-applicable` | 20 | 65387562.5 | `nanoseconds` | `validated` | yes |
| 5 | `call-bytecode-a4-template` | `call-bytecode-arity-4` | `template` | `not-applicable` | 20 | 8004917 | `nanoseconds` | `validated` | yes |
| 6 | `call-bytecode-a4-production-tiered` | `call-bytecode-arity-4` | `production-tiered` | `not-applicable` | 20 | 7505250 | `nanoseconds` | `validated` | yes |
| 7 | `call-host-a1-interpreter` | `call-host-arity-1` | `interpreter` | `not-applicable` | 20 | 48657687 | `nanoseconds` | `validated` | yes |
| 8 | `call-host-a1-template` | `call-host-arity-1` | `template` | `not-applicable` | 20 | 7740146 | `nanoseconds` | `validated` | yes |
| 9 | `call-host-a1-production-tiered` | `call-host-arity-1` | `production-tiered` | `not-applicable` | 20 | 7424729.5 | `nanoseconds` | `validated` | yes |
| 10 | `kernel-method-call-monomorphic-interpreter` | `kernel-method-call-monomorphic` | `interpreter` | `not-applicable` | 15 | 642531417 | `nanoseconds` | `validated` | yes |
| 11 | `kernel-method-call-monomorphic-template` | `kernel-method-call-monomorphic` | `template` | `not-applicable` | 15 | 93785000 | `nanoseconds` | `validated` | yes |
| 12 | `kernel-method-call-monomorphic-production-tiered` | `kernel-method-call-monomorphic` | `production-tiered` | `not-applicable` | 15 | 62811291 | `nanoseconds` | `validated` | yes |
| 13 | `kernel-branch-phi-interpreter` | `kernel-branch-phi` | `interpreter` | `not-applicable` | 20 | 624638542 | `nanoseconds` | `validated` | yes |
| 14 | `kernel-branch-phi-template` | `kernel-branch-phi` | `template` | `not-applicable` | 20 | 13200500 | `nanoseconds` | `validated` | yes |
| 15 | `kernel-branch-phi-production-tiered` | `kernel-branch-phi` | `production-tiered` | `not-applicable` | 20 | 7140062 | `nanoseconds` | `validated` | yes |
| 16 | `kernel-boxed-double-property-interpreter` | `kernel-boxed-double-property` | `interpreter` | `not-applicable` | 15 | 530170917 | `nanoseconds` | `validated` | yes |
| 17 | `kernel-boxed-double-property-template` | `kernel-boxed-double-property` | `template` | `not-applicable` | 15 | 15251917 | `nanoseconds` | `validated` | yes |
| 18 | `kernel-boxed-double-property-production-tiered` | `kernel-boxed-double-property` | `production-tiered` | `not-applicable` | 15 | 15284833 | `nanoseconds` | `validated` | yes |
| 19 | `kernel-dense-array-interpreter` | `kernel-dense-array` | `interpreter` | `not-applicable` | 15 | 739413125 | `nanoseconds` | `validated` | yes |
| 20 | `kernel-dense-array-template` | `kernel-dense-array` | `template` | `not-applicable` | 15 | 12783875 | `nanoseconds` | `validated` | yes |
| 21 | `kernel-dense-array-production-tiered` | `kernel-dense-array` | `production-tiered` | `not-applicable` | 15 | 7679958 | `nanoseconds` | `validated` | yes |
| 22 | `kernel-numeric-leaf-interpreter` | `kernel-numeric-leaf` | `interpreter` | `not-applicable` | 20 | 190199208.5 | `nanoseconds` | `validated` | yes |
| 23 | `kernel-numeric-leaf-template` | `kernel-numeric-leaf` | `template` | `not-applicable` | 20 | 19386478.5 | `nanoseconds` | `validated` | yes |
| 24 | `kernel-numeric-leaf-production-tiered` | `kernel-numeric-leaf` | `production-tiered` | `not-applicable` | 20 | 12690417 | `nanoseconds` | `validated` | yes |
| 25 | `jit-compile-engine-target-template` | `jit-compile-engineJitTarget` | `template` | `not-applicable` | 100 | 8458.5 | `nanoseconds` | `validated` | yes |
| 26 | `jit-compile-numeric-leaf-template` | `jit-compile-engineNumericLeaf` | `template` | `not-applicable` | 100 | 11250 | `nanoseconds` | `validated` | yes |
| 27 | `jit-compile-numeric-leaf-optimizing` | `jit-compile-engineNumericLeaf` | `optimizing` | `not-applicable` | 100 | 100687.5 | `nanoseconds` | `validated` | yes |
| 28 | `memory-forced-full` | `memory-allocation-churn-forced-full` | `interpreter` | `not-applicable` | 5 | 1104375042 | `nanoseconds` | `validated` | yes |
| 29 | `module-module-entry-fresh-per-sample-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 152604 | `nanoseconds` | `validated` | yes |
| 30 | `module-module-entry-fresh-per-sample-template` | `module-phases-module-entry.mjs` | `template` | `fresh-per-sample` | 20 | 160500 | `nanoseconds` | `validated` | yes |
| 31 | `module-module-entry-fresh-per-sample-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `fresh-per-sample` | 20 | 147646 | `nanoseconds` | `validated` | yes |
| 32 | `module-module-entry-reused-across-samples-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `reused-across-samples` | 20 | 126416.5 | `nanoseconds` | `validated` | yes |
| 33 | `module-module-entry-reused-across-samples-template` | `module-phases-module-entry.mjs` | `template` | `reused-across-samples` | 20 | 120604 | `nanoseconds` | `validated` | yes |
| 34 | `module-module-entry-reused-across-samples-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `reused-across-samples` | 20 | 122021 | `nanoseconds` | `validated` | yes |
| 35 | `package-entry-fresh-per-sample-interpreter` | `module-phases-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 249687.5 | `nanoseconds` | `validated` | yes |
