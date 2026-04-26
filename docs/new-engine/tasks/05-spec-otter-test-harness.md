# Task 05 — Spec: `otter test` Engine Harness

## Goal

Write `docs/new-engine/specs/otter-test-harness.md`. This spec defines the
fixture format, suite layout, runner behavior, output schemas, and timeout /
heap / interrupt rules for the new engine's first-party test command.

This is the **command contract** for `otter test`. It is **not** a Jest-clone
contract. The audience is engine developers, CI, and release smoke checks.

## Scope

The spec must cover:

### 1. Suites

- `engine` — first-party VM/runtime fixtures owned by this repo.
  Lives under `tests/engine/` (path documented here, created by task `07`).
- `smoke` — short release smoke tests run on every CI job.
  Lives under `tests/smoke/`.
- `test262` — curated subset run through the same runtime, distinct from the
  full Test262 corpus runner. Lives under `tests/test262-curated/`.
  The full corpus continues to run under the dedicated
  `crates/otter-test262` runner; `otter test --suite test262` is for fast
  feedback only.

A suite is selected with `--suite <name>` (default: `engine`).

### 2. Fixture format

A fixture is a single `.js` or `.ts` file with optional metadata. Metadata
lives in a leading `/* otter-test:` block comment, parsed as TOML. Required
and optional fields:

- `name` — string, defaults to the path
- `kind` — `script` (default), `module`, `eval`, `check-only`
- `expect` — block:
  - `exit_code = <int>` (default `0`)
  - `stdout = "<exact string>"` (optional)
  - `stdout_contains = ["..."]` (optional)
  - `stderr_contains = ["..."]` (optional)
  - `throws = "<DiagnosticKind>::<code>"` (optional)
  - `value = "<json>"` (optional, completion value as JSON)
  - `timeout = "<duration>"` — declares the test must time out (optional)
  - `oom = true` — declares the test must hit heap cap (optional)
- `capabilities` — list of capability strings the runner must grant
  (default: none / all-deny)
- `flags` — array of CLI flags forwarded to the test invocation
  (`--timeout`, `--max-heap-bytes`, etc.)
- `tags` — free-form tags for filtering (e.g., `["string", "rope"]`)
- `requires` — engine feature gates this fixture relies on (`ts`, `regex`,
  `weakmap`, …). Missing features cause `skipped`, not `failed`.
- `bless = false` — opt in/out of `--bless` updates (default `true` for
  golden fixtures)

Golden output fixtures live next to their source as
`<fixture>.expected.txt` (or `.expected.json`). The runner diffs against
them; `--bless` rewrites them but only for fixtures whose `bless` is `true`.

### 3. Runner behavior

- Each test runs in a **fresh `Runtime`** built from the same public API as
  embedders use (no private VM hooks).
- A hung test cannot hang the suite: each test has a watchdog. Default
  per-test timeout is **20 s** (overridable per suite and per fixture).
  When the watchdog fires, the test is recorded as `Timeout`, the runtime
  is dropped, and the next test starts on a fresh runtime.
- Heap cap is set per test (default **256 MiB**). A test that exceeds the
  cap is recorded as `OutOfMemory` (catchable `RangeError` per the existing
  test262 convention; see `CLAUDE.md`).
- Capability denials are recorded as `CapabilityDenied` outcomes, distinct
  from `Failed`.
- Tests run **sequentially by default** (the foundation VM is single-
  threaded; parallelism may be added later, never as the default during
  foundation).
- Filtering: `--filter <pattern>` matches against the fixture path or
  declared `name` (substring match; `--filter-re` for regex).
- Determinism: fixture iteration order is sorted by path. Random seeds are
  fixed where the runner exposes them.

### 4. Outcomes

Every test produces exactly one of:

- `Passed`
- `Failed { reason }`
- `Timeout`
- `OutOfMemory`
- `CapabilityDenied { capability }`
- `Skipped { reason }`
- `Crash { reason }` — the runtime panicked or aborted; this **must** be
  rare and is treated as a CI hard-fail signal.

### 5. Output modes

- Default human output: per-test status line, summary at the end with
  `passed / failed / timeout / oom / skipped / crash`.
- `--json` mode: NDJSON (one JSON object per test) followed by a final
  `summary` object. Schema:

  ```json
  // per test
  {
    "kind": "test",
    "name": "engine/strings/concat-loop",
    "path": "tests/engine/strings/concat-loop.ts",
    "outcome": "Passed",
    "duration_ms": 12,
    "diagnostics": [],
    "stdout": "...",
    "stderr": "..."
  }
  // final
  {
    "kind": "summary",
    "suite": "engine",
    "passed": 100,
    "failed": 0,
    "timeout": 0,
    "oom": 0,
    "skipped": 1,
    "crash": 0,
    "duration_ms": 1532
  }
  ```

- `--json` is stable (covered by snapshot tests in the runner).
- Human output is **not** stable.

### 6. `--bless`

- Updates only golden artifacts (`*.expected.txt`, `*.expected.json`) for
  fixtures whose `bless` is `true`.
- Refuses to bless fixtures whose declared `expect` would still fail after
  update (e.g., `throws = "..."` with no thrown value).
- Prints a list of changed files.

### 7. Diagnostics in tests

- Tests that assert on a thrown diagnostic (`throws = "..."`) match against
  the structured `Diagnostic` returned by the public API, not against
  `Debug`-formatted strings.
- `Diagnostic` rendering used in golden files goes through the same
  formatter the CLI uses, so a fixture is what the user sees.

### 8. CI integration

- `otter test --suite engine --json` is the canonical CI invocation for
  engine fixtures.
- Test count, pass count, timeout count, and crash count are exported as
  CI metrics. (The CI plumbing itself is a later task; the spec just names
  the contract.)

### 9. Non-goals

- **Not** a user-facing test runner that competes with Jest/Vitest.
- **No** `describe`/`it` DSL in the foundation phase.
- **No** parallel execution by default.
- **No** snapshot testing beyond the simple golden-output mechanism above.

## Out of scope

- Implementing the runner. (Task `07` provides the minimal harness; later
  slices land features incrementally.)
- Wiring CI. (A later task once the binary exists.)

## Files / directories you may touch

- Create: `docs/new-engine/specs/otter-test-harness.md`
- Read-only: everything else

## Acceptance criteria

- Spec file exists and covers each of sections 1–9 above.
- Fixture metadata fields are listed with type, default, and required/optional.
- Outcome enum is enumerated.
- JSON schema for `--json` mode includes both per-test and summary objects.
- The non-goals section explicitly rejects `describe/it` and parallelism for
  the foundation phase.
- Spec is referenced from ADR-0003 (CLI section) and from
  `docs/new-engine/tasks/README.md` (already done).

## Verification commands

```bash
test -f docs/new-engine/specs/otter-test-harness.md
rg -n "Passed|Failed|Timeout|OutOfMemory|CapabilityDenied|Skipped|Crash" \
    docs/new-engine/specs/otter-test-harness.md
rg -n "fixture|--bless|--json|--suite" \
    docs/new-engine/specs/otter-test-harness.md
```

## Risks

- **Scope creep.** Resist adding `describe/it`, mocks, watch mode, or
  parallel runners. Reject in the non-goals section.
- **Fragile golden files.** Golden tests are powerful but rot. Keep golden
  files small, ideally single-purpose.
- **Hidden coupling to current `otter-test262` runner.** This harness must
  not call into the dedicated test262 crate; only the curated subset is
  consumed via the same fixture format.

## Next task

Proceed to [`06-spec-bytecode-dump-disasm-trace.md`](./06-spec-bytecode-dump-disasm-trace.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts: [`docs/new-engine/specs/otter-test-harness.md`](../specs/otter-test-harness.md)
- verification:
  - spec exists at `docs/new-engine/specs/otter-test-harness.md`.
  - `rg -n "Passed|Failed|Timeout|OutOfMemory|CapabilityDenied|Skipped|Crash"`
    — 27 mentions; every outcome is enumerated.
  - `rg -n "fixture|--bless|--json|--suite"` — 46 mentions; full
    fixture / runner contract covered.
- decisions locked:
  - suites: `engine` (`tests/engine/`), `smoke` (`tests/smoke/`),
    `test262` (`tests/test262-curated/`).
  - fixture metadata: TOML in `/* otter-test: ... */` block; full
    field list in §2.1.
  - golden files: `*.expected.txt`, `*.expected.stderr.txt`,
    `*.expected.json` next to fixture.
  - runner: fresh runtime per test, public API only, watchdog,
    sequential, deterministic, exit-code policy.
  - outcomes: 7 variants enumerated in §4.
  - `--json` schema (`schema_version: 1`, NDJSON per test + summary).
  - `--bless` rules (only fixtures with `bless = true`; refuses
    impossible blessings).
  - Non-goals: no `describe/it`, no parallelism by default, no Jest
    clone, no TAP/JUnit, no plugins.
