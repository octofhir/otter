# Test262 conformance baselines

This directory holds the project's published Test262 conformance
baselines. Each baseline lands as a pair of files named after the
engine commit that produced it:

- `<engine-commit>.json` — canonical machine-readable wire format
  (the shape every PR diff is taken against).
- `<engine-commit>.md` — human-readable summary that GitHub renders
  inline.

`main.json` / `main.md` are the canonical baseline for the `main`
branch. Slice 105's CI workflow rewrites them after every push to
`main`; PR builds diff against them but never overwrite them.

## Workflow

```sh
# 1. Run a sweep + write a baseline.
cargo run --release -p otter-test262 -- run \
    --output docs/new-engine/test262-baseline/$(git rev-parse HEAD).json

# 2. Diff against the previous baseline.
cargo run -p otter-test262 -- diff \
    docs/new-engine/test262-baseline/main.json \
    --current docs/new-engine/test262-baseline/$(git rev-parse HEAD).json
```

Sharded mode (CI):

```sh
# Runs 8 shards in parallel.
for i in 1 2 3 4 5 6 7 8; do
    cargo run --release -p otter-test262 -- run \
        --shard $i/8 \
        --output reports/shard-$i.json &
done
wait

# Merge into a single canonical report.
cargo run -p otter-test262 -- merge reports/shard-*.json \
    --output docs/new-engine/test262-baseline/$(git rev-parse HEAD).json
```

## Pin advances

`vendor/test262` is a `git submodule` pinned to a deliberate commit
on tc39/test262's `main` branch. Advancing the pin is a deliberate
commit that records:

- The upstream changelog excerpt.
- A fresh baseline (`main.json` / `main.md`) captured at the new pin.

Pin-advance PRs that arrive without a matching baseline update are
rejected by the bump workflow (slice 105).

## CI workflow

Slice 105 wires two GitHub Actions workflows:

- **`.github/workflows/test262.yml`** — runs on every PR + push to
  `main`. Sharded matrix `[1..8]` invoked through
  `bash scripts/test262-safe.sh --shard N/8`. The aggregate job
  merges the per-shard JSONs, runs
  `diff docs/new-engine/test262-baseline/main.json merged.json`,
  and posts the diff as a PR comment via
  `actions/github-script@v7`. Job exits non-zero on any
  regression. Crashes are highlighted in bold.
- **`.github/workflows/test262-baseline.yml`** — runs on push to
  `main`. Same sharded sweep; the publish job commits the merged
  baseline directly back to `main` (using the workflow's
  `contents: write` permission) so the diff target advances as
  the engine improves. Per-test budget on CI is 30 s
  (`TIMEOUT_MS=30000`); local development uses 5 s by default.

`scripts/test262-safe.sh` is the canonical entry point for both
workflows. It applies:

- `ulimit -v 4G` on Linux (OS-level virtual-memory backstop).
- `--max-heap-bytes 536870912` (512 MiB engine cap surfaced as
  catchable `RangeError`).
- `--timeout` from `TIMEOUT_MS` (CI: 30 s; local default: 5 s).
- Auto-init for `vendor/test262` if missing.
- One automatic re-execution on hard-kill exit codes
  (86 / 137 / 139).
- Refusal to launch on debug builds without `--allow-debug`.

### Investigating a regression locally

```sh
cargo run -p otter-test262 -- run --filter '<failing-path-substring>'
```

### Pin-update PR template

When advancing `vendor/test262`, the PR description must include:

1. The upstream changelog excerpt (commits between the previous
   pin and the new one).
2. A fresh baseline (`main.json` + `main.md`), captured with the
   new pin via `bash scripts/test262-safe.sh --output
   docs/new-engine/test262-baseline/main.json`.

Pin-only PRs that arrive without a matching baseline update will
fail the regression gate (the `failing_tests` set will shift
without the baseline catching up).

Spec links:

- ECMA-262: <https://tc39.es/ecma262/>
- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
- Master plan:
  [`../tasks/100-test262-conformance.md`](../tasks/100-test262-conformance.md)
