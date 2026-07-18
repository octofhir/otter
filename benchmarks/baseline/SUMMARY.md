# Otter Engine Baseline

- Commit: `c3a7d26f9f45ea1b3b7b800268b30503b1c9df75`
- Outer watchdog: `120000` ms (capture-only; record timeout remains null)
- Platform: `macos` / `aarch64` / `Darwin 25.5.0` / `aarch64`
- Toolchain: `rustc 1.96.0 (ac68faa20 2026-05-25) binary: rustc commit-hash: ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96 commit-date: 2026-05-25 host: aarch64-apple-darwin release: 1.96.0 LLVM version: 22.1.2`

| # | Capture id | Benchmark | JIT | Reuse | Samples | Primary | Unit | Status | Eligible |
| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- |
| 1 | `call-bytecode-a0-interpreter` | `call-bytecode-arity-0` | `interpreter` | `not-applicable` | 20 | 48599666.5 | `nanoseconds` | `validated` | yes |
| 2 | `call-bytecode-a0-template` | `call-bytecode-arity-0` | `template` | `not-applicable` | 20 | 7616291.5 | `nanoseconds` | `validated` | yes |
| 3 | `call-bytecode-a0-production-tiered` | `call-bytecode-arity-0` | `production-tiered` | `not-applicable` | 20 | 5249104 | `nanoseconds` | `validated` | yes |
| 4 | `call-bytecode-a4-interpreter` | `call-bytecode-arity-4` | `interpreter` | `not-applicable` | 20 | 64131562.5 | `nanoseconds` | `validated` | yes |
| 5 | `call-bytecode-a4-template` | `call-bytecode-arity-4` | `template` | `not-applicable` | 20 | 8138958 | `nanoseconds` | `validated` | yes |
| 6 | `call-bytecode-a4-production-tiered` | `call-bytecode-arity-4` | `production-tiered` | `not-applicable` | 20 | 5686812.5 | `nanoseconds` | `validated` | yes |
| 7 | `call-host-a1-interpreter` | `call-host-arity-1` | `interpreter` | `not-applicable` | 20 | 48232104 | `nanoseconds` | `validated` | yes |
| 8 | `call-host-a1-template` | `call-host-arity-1` | `template` | `not-applicable` | 20 | 7707542 | `nanoseconds` | `validated` | yes |
| 9 | `call-host-a1-production-tiered` | `call-host-arity-1` | `production-tiered` | `not-applicable` | 20 | 7039291 | `nanoseconds` | `validated` | yes |
| 10 | `kernel-method-call-monomorphic-interpreter` | `kernel-method-call-monomorphic` | `interpreter` | `not-applicable` | 15 | 637285333 | `nanoseconds` | `validated` | yes |
| 11 | `kernel-method-call-monomorphic-template` | `kernel-method-call-monomorphic` | `template` | `not-applicable` | 15 | 16107042 | `nanoseconds` | `validated` | yes |
| 12 | `kernel-method-call-monomorphic-production-tiered` | `kernel-method-call-monomorphic` | `production-tiered` | `not-applicable` | 15 | 15819958 | `nanoseconds` | `validated` | yes |
| 13 | `kernel-branch-phi-interpreter` | `kernel-branch-phi` | `interpreter` | `not-applicable` | 20 | 623016583.5 | `nanoseconds` | `validated` | yes |
| 14 | `kernel-branch-phi-template` | `kernel-branch-phi` | `template` | `not-applicable` | 20 | 12833625 | `nanoseconds` | `validated` | yes |
| 15 | `kernel-branch-phi-production-tiered` | `kernel-branch-phi` | `production-tiered` | `not-applicable` | 20 | 7144729.5 | `nanoseconds` | `validated` | yes |
| 16 | `kernel-boxed-double-property-interpreter` | `kernel-boxed-double-property` | `interpreter` | `not-applicable` | 15 | 510923458 | `nanoseconds` | `validated` | yes |
| 17 | `kernel-boxed-double-property-template` | `kernel-boxed-double-property` | `template` | `not-applicable` | 15 | 15227750 | `nanoseconds` | `validated` | yes |
| 18 | `kernel-boxed-double-property-production-tiered` | `kernel-boxed-double-property` | `production-tiered` | `not-applicable` | 15 | 10247458 | `nanoseconds` | `validated` | yes |
| 19 | `kernel-dense-array-interpreter` | `kernel-dense-array` | `interpreter` | `not-applicable` | 15 | 736738042 | `nanoseconds` | `validated` | yes |
| 20 | `kernel-dense-array-template` | `kernel-dense-array` | `template` | `not-applicable` | 15 | 12973250 | `nanoseconds` | `validated` | yes |
| 21 | `kernel-dense-array-production-tiered` | `kernel-dense-array` | `production-tiered` | `not-applicable` | 15 | 7685334 | `nanoseconds` | `validated` | yes |
| 22 | `kernel-numeric-leaf-interpreter` | `kernel-numeric-leaf` | `interpreter` | `not-applicable` | 20 | 187481625 | `nanoseconds` | `validated` | yes |
| 23 | `kernel-numeric-leaf-template` | `kernel-numeric-leaf` | `template` | `not-applicable` | 20 | 19287854.5 | `nanoseconds` | `validated` | yes |
| 24 | `kernel-numeric-leaf-production-tiered` | `kernel-numeric-leaf` | `production-tiered` | `not-applicable` | 20 | 10614291.5 | `nanoseconds` | `validated` | yes |
| 25 | `jit-compile-engine-target-template` | `jit-compile-engineJitTarget` | `template` | `not-applicable` | 100 | 7937.5 | `nanoseconds` | `validated` | yes |
| 26 | `jit-compile-numeric-leaf-template` | `jit-compile-engineNumericLeaf` | `template` | `not-applicable` | 100 | 11791 | `nanoseconds` | `validated` | yes |
| 27 | `jit-compile-numeric-leaf-optimizing` | `jit-compile-engineNumericLeaf` | `optimizing` | `not-applicable` | 100 | 102271 | `nanoseconds` | `validated` | yes |
| 28 | `memory-forced-full` | `memory-allocation-churn-forced-full` | `interpreter` | `not-applicable` | 5 | 1097056834 | `nanoseconds` | `validated` | yes |
| 29 | `memory-runtime-idle` | `memory-runtime-idle` | `interpreter` | `fresh-per-sample` | 5 | 13123542 | `nanoseconds` | `validated` | yes |
| 30 | `module-module-entry-fresh-per-sample-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 146020.5 | `nanoseconds` | `validated` | yes |
| 31 | `module-module-entry-fresh-per-sample-template` | `module-phases-module-entry.mjs` | `template` | `fresh-per-sample` | 20 | 149625.5 | `nanoseconds` | `validated` | yes |
| 32 | `module-module-entry-fresh-per-sample-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `fresh-per-sample` | 20 | 137729 | `nanoseconds` | `validated` | yes |
| 33 | `module-module-entry-reused-across-samples-interpreter` | `module-phases-module-entry.mjs` | `interpreter` | `reused-across-samples` | 20 | 125541.5 | `nanoseconds` | `validated` | yes |
| 34 | `module-module-entry-reused-across-samples-template` | `module-phases-module-entry.mjs` | `template` | `reused-across-samples` | 20 | 146312.5 | `nanoseconds` | `validated` | yes |
| 35 | `module-module-entry-reused-across-samples-production-tiered` | `module-phases-module-entry.mjs` | `production-tiered` | `reused-across-samples` | 20 | 126625 | `nanoseconds` | `validated` | yes |
| 36 | `package-entry-fresh-per-sample-interpreter` | `module-phases-entry.mjs` | `interpreter` | `fresh-per-sample` | 20 | 354353.5 | `nanoseconds` | `validated` | yes |
