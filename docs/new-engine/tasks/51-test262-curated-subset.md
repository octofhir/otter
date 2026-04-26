# Task 51 — Curated Test262 subset wired into `otter test`

## Goal

Bring up the `otter test --suite test262` harness path from spec
(`docs/new-engine/specs/otter-test-harness.md`) with a small
curated Test262 subset committed under
`tests/test262-curated/`.

## Scope

- Create `tests/test262-curated/` with a starter set of fixtures
  copied from the upstream Test262 corpus (literal `.js` files)
  for the surfaces the engine already supports: integer arithmetic,
  string methods, control flow, function calls.
- Each file is **runnable by the existing harness** without
  per-fixture wrapping — the harness reads the leading
  `/* otter-test: ... */` block, exactly like the engine fixtures.
- Where Test262 expects a thrown error, the harness fixture
  declares `expect.throws = "..."`.
- Add a `--filter` argument example to the spec referencing the
  curated subset.
- Record the baseline pass count in the task's closure notes.

## Out of scope

- Running the full Test262 corpus (the `crates/otter-test262`
  legacy crate stays out of scope for now).
- Auto-generating the curated set from the corpus.

## Files / directories you may touch

- `tests/test262-curated/`
- `crates-next/otter-test/` (only if the harness needs a tweak to
  read these fixtures correctly).

## Acceptance criteria

- `otter test --suite test262` runs the curated set without
  crashes.
- Pass / fail counts are stable across runs (no flakes).
- README index entry for this task is removed when the work is
  closed.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite test262
```

## Risks

- Test262 corpus fixtures expect a real `assert` / `assertEq`
  helper. The harness ships a small one as a preamble script
  appended to each fixture.

## Status

- not started
