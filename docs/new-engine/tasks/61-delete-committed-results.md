# Task 61 — Delete committed result / scratch artifacts

## Goal

Clean committed test-result dumps, vendored `node_modules`, and
ad-hoc scripts at the repo root. Extend `.gitignore` so they
cannot creep back in.

## Scope

- Delete:
  - `test262_results/` — committed JSONL dumps from the legacy
    runner.
  - `benchmarks/results/` — committed bench output.
  - `benchmarks/node_modules/` — vendored npm install.
  - `benchmarks/c2-strings-latest.log`.
  - `scratch/check_sizes.rs` and the now-empty directory.
  - `run-benchmark.sh`, `run-bun-benchmark.sh`,
    `test-server.sh` at the repo root.
- Extend `.gitignore`:
  - `test262_results/`
  - `benchmarks/results/`
  - `benchmarks/node_modules/`
  - `*.cpuprofile`
  - `*.heapsnapshot`
  - `*.trace.json`
  - `*.folded`
  - `timeout-dump*.txt`
  - `scratch/`

## Out of scope

- `crates/*` legacy directories — stay on disk.
- `benchmarks/` other contents — out of scope.

## Files / directories you may touch

- The deletion targets above.
- `.gitignore`.

## Acceptance criteria

- The targets are gone.
- `.gitignore` lists the new patterns.
- `git status` is clean after the cleanup commit.

## Verification commands

```bash
test ! -d test262_results
test ! -d benchmarks/results
test ! -d benchmarks/node_modules
test ! -d scratch
test ! -f run-benchmark.sh
grep -q "test262_results/" .gitignore
```

## Risks

- The deletion is destructive — make sure no in-flight CI job
  consumes these files. Check the workflow files first.

## Status

- not started
