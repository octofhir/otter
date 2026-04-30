# Task 104 — Test262 baseline + diff

**Parent:** [100 — Test262 conformance](./100-test262-conformance.md).
**Predecessor:** [103 — Outcomes + negative](./103-test262-outcomes-and-negative.md).
**Successor:** [105 — CI integration](./105-test262-ci-integration.md).

## Scope

Drive the full corpus with sharding, write versioned JSON +
Markdown reports under `docs/new-engine/test262-baseline/`, and
implement the `--diff` subcommand that reports
regressions / improvements vs an earlier baseline.

## Why

Without baseline files there is nothing for CI (slice 105) to
diff against, and nothing the project can publish. The diff
subcommand is the regression-detection primitive every later PR
will rely on.

## Deliverables

1. **Sharding** — `--shard N/M` splits the corpus by stable hash
   so each shard is independent of corpus order. Default: no
   sharding (full run on one box). Mode `merge <reports/*.json>`
   combines shard outputs into one canonical report.
2. **JSON report writer** at
   `docs/new-engine/test262-baseline/<engine-commit>.json`. Shape
   matches task 100 §"Output formats". Required fields:
   `test262_commit`, `engine_commit`, `ran_at`, `totals`,
   `by_section`, `failing_tests`. The `by_section` map keys are
   directory prefixes
   (`language/expressions/addition`, `built-ins/Array/prototype/flat`,
   etc.) — the runner derives them from the test path so the
   slicing keeps working as new tests land.
3. **Markdown report writer** at
   `docs/new-engine/test262-baseline/<engine-commit>.md`. Top
   block: totals + pass-rate. Middle block: top-50 failing
   sections with their pass rate. Bottom block: top-100
   failing-test patterns, deduplicated by `reason`-prefix.
4. **Both files commit together** so `git blame` on the JSON
   shows the engine commit + the test262 commit.
5. **`diff <previous>` subcommand** — loads the previous JSON
   baseline and the freshly-written one, reports:
   - `+N` newly passing
   - `-N` regressed (was Pass, now anything else; or was
     Skipped, now Crash)
   - `±0` unchanged
   Format matches task 100 §"`--diff <previous>` mode" sample.
   Exit code: `0` on no regressions; `1` on any regression.
6. **`merge <reports/*.json>` subcommand** — combines shard
   outputs by union (each test appears in exactly one shard
   per the stable-hash split). Validates that no test appears
   twice; surfaces collisions as a hard error.
7. **`docs/new-engine/test262-baseline/README.md`** —
   one-page index explaining the directory's purpose, the diff
   workflow, and the policy that baselines only land when
   `main` updates (PR runs produce diffs but never overwrite
   the baseline; only post-merge CI does).

## Files to touch

- `crates-next/otter-test262/src/report.rs`
- `crates-next/otter-test262/src/diff.rs`
- `crates-next/otter-test262/src/shard.rs`
- `crates-next/otter-test262/src/main.rs` (subcommand wiring)
- `docs/new-engine/test262-baseline/README.md` (new)

## Sequencing notes

- 103 must land first — this slice consumes `TestResult`.
- 105 imports the diff subcommand and the merge subcommand.
- The first baseline lands as a separate post-merge PR after
  104 ships; CI's regression gate (105) gets enabled only once
  the baseline file is in `main`.

## Gates

- `cargo run -p otter-test262 -- run --filter
  'test/built-ins/Math/**' --output /tmp/out.json` produces a
  schema-valid JSON report.
- `cargo run -p otter-test262 -- diff /tmp/out.json` against
  itself reports `±0` and exits 0.
- A synthesised regression (hand-edited JSON flipping one Pass
  → Fail) makes `diff` report `-1` and exit non-zero.
- `cargo run -p otter-test262 -- run --shard 1/4 --output
  shard1.json` … `--shard 4/4 --output shard4.json` followed by
  `merge shard*.json --output merged.json` produces a report
  identical (modulo `ran_at`) to a single-shard run.
- Markdown report renders cleanly in GitHub's preview.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Spec links

- ECMA-262 §-section anchoring used by the `by_section` map:
  <https://tc39.es/ecma262/>
- Test262 directory layout:
  <https://github.com/tc39/test262/blob/main/CONTRIBUTING.md#directory-layout>
- ADR-0001:
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)
