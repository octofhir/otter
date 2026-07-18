---
title: "Engine Baselines"
---

Otter performance work starts from one clean, reproducible engine baseline.
The baseline is intentionally narrower than compatibility suites: it measures
the VM, runtime, GC, and JIT without installing packages or bootstrapping
Node/Web product surfaces.

## The measured matrix

`otter-engine-baseline` owns an ordered 35-case matrix:

- bytecode calls with zero and four arguments under `interpreter`, `template`,
  and `production-tiered`;
- an extracted native host call under the same three policies;
- five JavaScript kernels under the same three policies, including a
  straight-line numeric leaf;
- direct template compilation plus isolated template and optimizing
  numeric-leaf compilation; every final measured artifact is installed,
  entered with explicit numeric arguments, and required to return the expected
  result;
- managed allocation churn followed by a forced full GC;
- fresh and reused module runtimes under all three tier policies;
- package-import-map resolution in an isolated interpreter runtime.

Every result retains its raw samples and typed aggregate. Tier selection is a
command argument; legacy environment modes do not participate.

## Capture

Run from an otherwise idle host and a clean commit:

```sh
cargo run --locked --release -p otter-benchmark --features engine \
  --bin otter-engine-baseline -- capture
```

The driver builds `otter-engine-benchmark` in release mode, rechecks the same
clean HEAD, then runs all cases serially. Evidence is written only beneath the
ignored `benchmarks/results/` tree, so later children still observe a clean
worktree.

Each capture contains:

```text
engine-<commit>-<timestamp>/
  capture.json
  SUMMARY.md
  records/
  raw/
```

`capture.json` is the one unversioned capture manifest. It records the ordered
argv vectors, real child exit codes, raw/record paths, and the outer watchdog.
The watchdog is capture orchestration, not an engine sampling setting:
successful child records must retain `sampling.timeoutMs: null`. If it fires,
the driver preserves raw output and records `outer-timeout` in the manifest;
it never invents a result or score.

For an exact policy surface, capture rejects non-empty `OTTER_JIT*`,
`OTTER_GC*`, and `RUST_LOG` variables.

## Publish

After inspecting a successful ignored capture, publish it explicitly:

```sh
cargo run --locked --release -p otter-benchmark --features engine \
  --bin otter-engine-baseline -- publish \
  --capture benchmarks/results/engine-<commit>-<timestamp>
```

Publication re-parses the exact child JSON, checks the live result contract,
recomputes aggregates, validates the fixed identity/configuration/sampling
matrix, and requires one clean release commit plus one platform and toolchain
across all rows. It also regenerates and byte-compares `SUMMARY.md`.

Only then does it atomically create the one current
`benchmarks/baseline/` directory. It refuses an existing destination or any
failed, timed-out, unvalidated, dirty, debug, edited, or incomplete capture.
Archived Phase 0 data is never read as a fallback.

## Comparing changes

Compare rows only when workload identity, parameters, tier, runtime reuse,
sample count, warmup count, unit, direction, and aggregation statistic match.
Use the interpreter row as the semantic/no-native oracle, the template row to
isolate baseline code generation, and the production-tiered row to evaluate
promotion and optimizer effects.

The baseline establishes timing evidence; it is not a sampling profiler. For
wrong-code or tier-transition diagnosis, use the
[JIT debugging workflow](/otter/engine/jit-debugging/) and correlate events,
bytecode, normalized code, annotated assembly, relocations, deopt metadata,
and safepoints.
