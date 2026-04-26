# Task 53 — Recreate `ES_CONFORMANCE.md`

## Goal

Once the curated Test262 subset (task 51) reports a stable
baseline, recreate the top-level `ES_CONFORMANCE.md` document with
the current pass / fail rates per feature area.

## Scope

- One row per feature area (strings, numbers, arrays, control
  flow, classes, async, etc.).
- Each row lists `passed / total / pass-rate` from the curated
  subset run.
- Add a "How to update" section pointing at
  `cargo run -p otter-cli -- test --suite test262 --json`.
- Add a "Known gaps" section summarising deferred slices.

## Out of scope

- Full Test262 corpus pass-rate (that would depend on the legacy
  `crates/otter-test262` runner being revived).

## Files / directories you may touch

- `ES_CONFORMANCE.md` (new at repo root).

## Acceptance criteria

- `ES_CONFORMANCE.md` lives at the repo root.
- The numbers in it match a fresh `otter test --suite test262
  --json` run from the time of writing.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite test262 --json | \
    jq 'select(.kind == "summary")'
```

## Risks

- Depends on task 51; do not start until that task closes.

## Status

- not started
