# One owner for arithmetic exit repair

Investigation and closing validation: September 25, 2026.

Local Apple M1/macOS AArch64 release investigation based on signed commit
`0de92369e8c4e6a695ef255a1bcff561ec2b787e`. Development observations are not a
published engine baseline. The new `benchmarks/scripts/arith-exit-repair.js`
has SHA-256 `947be0176ae0ff2c408df9bc1afa576487fff686af50cc49363b93a66007f102`
and returns exactly `150000` on every invocation.

## Starting point

An Int32-specialized arithmetic site exits with `int32Overflow` or
`negativeZero` when its result leaves Int32. Only two paths repaired that
speculation: the OSR dispatcher and generated-linkage deopts widened `Add`,
`Sub` and `Mul` feedback and recompiled. A whole-function entry exit and a
deopt out of an inlined callee did neither: `note_jit_optimized_bail`
returned early for both reasons, leaving the repair to the OSR dispatcher it
never reached. `Neg`, `Increment`, `AddImm` and `SubImm` share the same
arithmetic feedback but were never widened at all.

The kernel negates lane 0 (`-0`) on the first iteration of every call. Before
this slice, ProductionTiered entered its optimized code 28 times over 28
invocations and exited 28 times, each time finishing the 200,000-iteration
loop in the interpreter: 43.8 ms against Template's 3.2 ms.

## Change

`Interpreter::note_jit_optimized_bail_at` is now the one owner of optimizing
exits. For an `Int32Overflow` / `NegativeZero` exit from `Add`, `Sub`, `Mul`,
`Neg`, `Increment`, `AddImm` or `SubImm`, it widens that site's arithmetic
feedback to Number once and retires the generation. Entry, OSR,
generated-linkage and inlined-deopt exits all reach it:

- the OSR dispatcher no longer repeats the repair for optimizing exits (it
  still repairs Template exits);
- generated linkage drops its private repair call;
- an inlined deopt passes the innermost spliced frame and its resume PC, so
  the callee's site widens, not the caller's call site.

A repeated exit at an already-widened site takes the ordinary exit policy.

## Final paired result

Three directly alternating pairs per tier, eight warmups and twenty samples,
pair 2 reversed. The before binary is the previous slice's gate build
(`05da46b9…`). The after binary is this slice's closing-gate build
(`dc3e1d78…`). All 18 processes validate checksum `150000`. Load average was
about 3.5; no builds, tests or agents ran.

| Tier | Pair 1 before → after, ms | Pair 2 before → after, ms | Pair 3 before → after, ms | Paired wall change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 39.782 → 39.636 | 39.825 → 39.750 | 39.792 → 39.652 | −0.30% (neutral) |
| Template | 3.273 → 3.159 | 3.163 → 3.158 | 3.170 → 3.241 | −0.48% (neutral) |
| ProductionTiered | 43.789 → 2.290 | 43.954 → 2.392 | 43.710 → 2.295 | **−94.69%** |

| Paired process counters | Interpreter | Template | ProductionTiered |
| --- | ---: | ---: | ---: |
| Retired instructions | −0.00% | −0.06% | **−94.20%** |
| Hardware cycles | +0.06% | −0.68% | −93.69% |

| ProductionTiered totals across 28 invocations | Before | After |
| --- | ---: | ---: |
| Optimized entries | 28 | 30 |
| Optimizing deopts | 28 | 2 |
| Code generations | 1 | 3 |
| Emitted code bytes | 2,728 | 8,120–8,144 |
| Runtime stub transitions | 0 | 1,366 |
| Allocated cells / bytes | 114 / 5,736 | same |

The two remaining deopts are the one-time widenings (the `-0` negation and
the accumulator leaving Int32), each followed by a recompile. The runtime stub
count after the change equals Template's 1,367: the optimized code now runs
the whole loop, where before it exited on the first iteration. Production now
runs the kernel in 2.3 ms, 28% faster than Template's 3.2 ms, instead of 13.7x
slower.

At **41 hand-written production Rust lines added** (21 removed, excluding
comments, tests, benchmarks and documentation), the wall gain is **2.31
percentage points per added line for ProductionTiered**.

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger
  (`arith-exit-repair` Template 3.16 ms, Production 2.28 ms, zero deopts per
  warmed invocation).
- Focused runtime matrix of 24 test binaries: 94/94 on AArch64 and 89/89
  under Rosetta x86-64 at each of `OTTER_GC_STRESS` unset, 16, 4 and 1.
- Machine suites 173/173 (AArch64) and 168/168 (x86-64).
- Targeted Test262 on AArch64 and Rosetta with zero failures, crashes,
  timeouts or OOMs: Math 327/327, Number 339/340 (1 skipped), unary-minus
  14/14, addition 48/48, subtraction 38/38, multiplication 40/40,
  prefix-increment 33/33, postfix-increment 38/38, compound-assignment
  454/454, `expressions/call` 91/92 (1 skipped), String 1331/1334 (3
  skipped).
- `git diff --check` passed.

New regression `crates/otter-runtime/tests/jit_arith_exit_repair.rs`:
- entry exits from `Neg` producing `-0` and from `Increment` / `Add` leaving
  Int32 widen once and keep entering optimized code;
- the same exits from callees spliced into an optimized caller widen the
  innermost site;
- the unmodified kernel stays optimized. The before binary shows one deopt
  per invocation on this kernel, which exceeds the test's bound of four.

## Reproduction

Build `otter-engine-benchmark` with `cargo build --locked --release
-p otter-benchmark --features engine --bin otter-engine-benchmark`, keep
separate before/after executables, and run each tier serially with
`OTTER_GC_STRESS` unset:

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/arith-exit-repair.js \
  --function engineKernel --expected 150000 \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-arith-exit-repair-environment.json),
[run counters](2026-09-25-arith-exit-repair-runs.csv) and
[wall samples](2026-09-25-arith-exit-repair-samples.csv) are tracked here.
Binaries, logs, the pre-gate pilot (excluded from the result) and Test262
records are retained under ignored `benchmarks/results/negzero-2026-09-25/`.
