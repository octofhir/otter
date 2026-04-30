# Task 105 — Test262 CI integration

**Parent:** [100 — Test262 conformance](./100-test262-conformance.md).
**Predecessor:** [104 — Baseline + diff](./104-test262-baseline-and-diff.md).

## Scope

Wire the runner into GitHub Actions: PR builds run a sharded
sweep, post the diff against `main`'s baseline as a comment, and
fail on regressions. Post-merge runs on `main` write a fresh
baseline.

## Why

Without CI the runner is theatre — engineers will not run a
30-minute conformance sweep by hand on every PR. Automation +
publishing the diff in the PR review surface is what enforces
the regression gate.

## Deliverables

1. **`.github/workflows/test262.yml`** with two jobs:
   - **`shard`** — matrix `[1..8]`. Each shard runs on a 32-core
     runner under
     `bash scripts/test262-safe.sh --shard ${{ matrix.shard }}/8
     --output reports/shard-${{ matrix.shard }}.json` and uploads
     the JSON shard as an artifact.
   - **`aggregate`** — `needs: [shard]`. Downloads all shard
     artifacts, runs
     `cargo run -p otter-test262 --release -- merge
     reports/*.json --output merged.json`, then
     `diff docs/new-engine/test262-baseline/main.json merged.json`.
     Posts the diff body as a PR comment via
     `actions/github-script@v7`. Job exits non-zero on any
     regression.
2. **`scripts/test262-safe.sh`** — wraps the runner with the
   safety controls from task 100 §"Safety controls":
   `ulimit -v 4G` on Linux, `--max-heap-bytes 536870912` per
   test, `--timeout 30000` per test. Refuses to launch in debug
   builds without `--allow-debug`.
3. **Baseline-bump workflow** —
   `.github/workflows/test262-baseline.yml` triggered on push
   to `main`. Runs the same sharded sweep, merges the result,
   commits it as
   `docs/new-engine/test262-baseline/main.json` and `.md` via a
   PR (using `peter-evans/create-pull-request`). The baseline
   file's commit message embeds the test262 SHA + engine SHA
   for traceability.
4. **PR-comment template** — Markdown block with:
   - Total pass rate (this PR vs `main`).
   - Top-10 newly-failing tests (path + reason).
   - Top-10 newly-passing tests.
   - Skipped count delta.
   - Crash count (any non-zero is highlighted in bold red).
   - Link to the full Markdown report artifact.
5. **Pin-update PR template** — when a contributor wants to
   advance `vendor/test262`, the PR description must include
   the upstream changelog excerpt and a fresh baseline. The
   bump workflow rejects pin advances that arrive without a
   matching baseline update.
6. **Documentation** —
   `docs/new-engine/test262-baseline/README.md` extended with
   a §"CI workflow" subsection explaining the gate, the bump
   procedure, and how to investigate a regression locally
   (`cargo run -p otter-test262 -- run --filter '<failing-path>'`).

## Files to touch

- `.github/workflows/test262.yml` (new)
- `.github/workflows/test262-baseline.yml` (new)
- `scripts/test262-safe.sh` (new — port from `crates/`-archive
  reference; do not import the parked-tree script directly)
- `docs/new-engine/test262-baseline/README.md` (extend)

## Sequencing notes

- 104 must land + the first baseline must be committed on
  `main` before the regression gate flips on. Ship 105 in two
  PRs: PR-A wires the workflow with `continue-on-error: true`
  on the diff step (so the workflow is observable but does not
  block); PR-B drops the `continue-on-error` once the first
  baseline is on `main` and the diff is reliably empty.
- Sharding count (8) is deliberate — fits a 32-core runner with
  4-core-per-shard headroom.

## Gates

- A PR with a hand-introduced regression triggers the gate;
  reverting it makes CI green.
- A PR that does not touch `crates-next/*` shows the unchanged
  diff with `±0`.
- The PR comment renders correctly in GitHub's UI (no escape
  bugs in the Markdown).
- `bash scripts/test262-safe.sh --filter
  'test/built-ins/Math/**'` runs to completion locally on
  Linux with the heap cap and timeout in effect.
- The baseline-bump workflow successfully commits a fresh
  `main.json` + `main.md` after a force-push to a fork.
- `cargo clippy --workspace --all-targets -- -D warnings`
  clean (no Rust changes here, but the workflow may add
  `cargo-deny` invocations — keep them lint-clean too).

## Spec links

- GitHub Actions PR comment best practices:
  <https://docs.github.com/en/actions/using-workflows/events-that-trigger-workflows#pull_request>
- Test262 contribution guide (pin-update etiquette):
  <https://github.com/tc39/test262/blob/main/CONTRIBUTING.md>
- ADR-0001:
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)
