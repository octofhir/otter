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

- **`test262.yml`** — runs on every PR + push to `main`. Sharded
  matrix `[1..8]`; the aggregate job merges the per-shard JSONs,
  runs `diff main.json merged.json`, and posts the diff as a PR
  comment. Exits non-zero on any regression.
- **`test262-baseline.yml`** — runs on push to `main`. Same sharded
  sweep, but commits the merged baseline back to `main` so the
  diff target moves forward as the engine improves.

Investigating a regression locally:

```sh
cargo run -p otter-test262 -- run --filter '<failing-path-substring>'
```

Spec links:

- ECMA-262: <https://tc39.es/ecma262/>
- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
- Master plan:
  [`../tasks/100-test262-conformance.md`](../tasks/100-test262-conformance.md)
