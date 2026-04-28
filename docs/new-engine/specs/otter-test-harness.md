# SPEC — `otter test` Engine Harness

- **Status:** accepted
- **Date:** 2026-04-26
- **Related:**
  - [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
    §M11
  - [`docs/new-engine/adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
  - [task 05](../tasks/05-spec-otter-test-harness.md)

## Purpose

`otter test` is the **engine harness command**: a first-party runner
for `.js` and `.ts` fixtures that drives the new runtime through the
**same public API embedders use** (ADR-0003). It is **not** a
Jest/Vitest replacement, **not** a user test framework, and **not** a
generic test runner.

Audience:

- engine developers writing slice fixtures;
- CI gates;
- release smoke checks.

## 1. Suites

A suite is a named collection of fixtures rooted at a fixed path.
`otter test --suite <name>` selects which collection to run. Default
suite: `engine`.

| Suite | Path | Purpose |
| --- | --- | --- |
| `engine` | `tests/engine/` | First-party VM/runtime fixtures owned by this repo. Slice tasks add fixtures here. |
| `smoke` | `tests/smoke/` | Short release smoke tests (≤ 30 s wall-clock total). Run on every CI job. |
| `test262` | `tests/test262-curated/` | Curated subset of Test262 run through the same runtime. The full Test262 corpus continues to use a separate runner; this suite is for fast feedback loops. |

`tests/engine/` and `tests/smoke/` are created by foundation tasks
(`07`–`13`). `tests/test262-curated/` is created later, when the
first slice that exercises Test262 fixtures lands (≥ task `09`).

## 2. Fixture format

A fixture is a single `.js` or `.ts` file. Optional metadata lives in
a leading block comment of the form `/* otter-test: ... */`, parsed
as TOML. Multi-line TOML inside the block is allowed.

### 2.1 Metadata fields

| Field | Type | Default | Required | Purpose |
| --- | --- | --- | --- | --- |
| `name` | `string` | fixture path | no | Human-readable identifier used in filters and output. |
| `kind` | `"script" \| "module" \| "eval" \| "check-only"` | `"script"` | no | Selects which `Runtime` method drives the fixture. |
| `expect.exit_code` | `int` | `0` | no | Required exit code. |
| `expect.stdout` | `string` | — | no | Exact-match stdout. |
| `expect.stdout_contains` | `[string]` | — | no | Substrings that must appear in stdout. |
| `expect.stderr_contains` | `[string]` | — | no | Substrings that must appear in stderr. |
| `expect.throws` | `string` (form `"Kind::CODE"`) | — | no | Asserts a structured `Diagnostic` with the given kind and code. |
| `expect.value` | `string` (JSON) | — | no | Expected completion value (JSON-encoded for stable comparison). |
| `expect.timeout` | `string` (duration) | — | no | Asserts the fixture must time out at the given duration. |
| `expect.oom` | `bool` | `false` | no | Asserts the fixture must hit the heap cap. |
| `capabilities` | `[string]` | `[]` | no | Capability strings the runner must grant before running. |
| `flags` | `[string]` | `[]` | no | CLI flags forwarded to the test invocation (e.g., `["--max-heap-bytes", "65536"]`). |
| `tags` | `[string]` | `[]` | no | Free-form tags for filtering. |
| `requires` | `[string]` | `[]` | no | Engine feature gates the fixture relies on (`ts`, `regex`, `weakmap`, …). Missing features cause the fixture to be `Skipped`, not `Failed`. |
| `bless` | `bool` | `true` for fixtures with golden output, else `false` | no | Whether `--bless` is allowed to update this fixture's golden files. |

Each `expect.*` field is independent; any combination is allowed.
A fixture with no metadata block runs as a `script` and asserts only
`exit_code = 0`.

### 2.2 Golden files

Golden output files live next to the fixture:

- `<fixture>.expected.txt` — exact stdout match.
- `<fixture>.expected.stderr.txt` — exact stderr match.
- `<fixture>.expected.json` — when the fixture produces a structured
  artifact (e.g., a `--dump-bytecode=json` golden).

The runner diffs against the relevant golden file. `--bless` rewrites
the golden file if and only if the fixture's `bless` is true.

### 2.3 Example

```typescript
/* otter-test:
name = "string concat loop is O(n)"
kind = "script"
flags = ["--max-heap-bytes", "65536000"]
tags = ["string", "rope"]
requires = ["string-core"]

[expect]
exit_code = 0
stdout_contains = ["len=4000"]
*/

let s = "";
for (let i = 0; i < 1000; i++) {
  s += "abcd";
}
console.log("len=" + s.length);
```

## 3. Runner behavior

- **Fresh runtime per test.** Each fixture is executed in a brand-new
  `Runtime` built from the public `RuntimeBuilder`. No state leaks
  across fixtures.
- **No private VM hooks.** The runner uses only the API documented in
  ADR-0003. If a fixture needs non-public observability, the spec
  there is amended first; the runner is updated second.
- **Watchdog.** Every test has a per-test timeout. When it fires:
  - the runtime is dropped;
  - the test is recorded as `Timeout`;
  - the next test starts with a fresh runtime.
- **Default timeouts.** 20 s per test in `engine`, 5 s per test in
  `smoke`, 10 s per test in `test262`. Overridable per suite (suite
  configuration file) and per fixture (`flags = ["--timeout", "..."]`
  or `expect.timeout`).
- **Heap cap.** 256 MiB default per test (matches `RuntimeBuilder`
  default). A fixture that exceeds the cap is recorded as
  `OutOfMemory` (not `Failed` and not `Crash`); see `RuntimeError::OutOfMemory`
  per ADR-0003.
- **Capability denials** are recorded as `CapabilityDenied { capability }`
  and are distinct from `Failed`.
- **Sequential execution.** Tests run sequentially in a single thread.
  Parallelism may be added later, never as a default during foundation
  (the new VM is single-threaded; one runtime per thread is the
  contract per ADR-0003).
- **Filtering.** `--filter <pattern>` matches against the fixture
  path or declared `name` (substring match). `--filter-re <regex>`
  for regex match. `--tag <tag>` selects fixtures by tag; repeatable.
- **Determinism.** Fixture iteration order is sorted by path. Random
  seeds (when the runner exposes them) are fixed.
- **Exit code.** The runner exits `0` if all tests are `Passed` or
  `Skipped`; `1` if any test is `Failed`, `Timeout`, or
  `OutOfMemory` against expectation, or `CapabilityDenied`
  unexpectedly; `64+` if any test produces `Crash` (CI hard-fail).

## 4. Outcomes

Every test produces exactly one outcome:

| Outcome | Meaning |
| --- | --- |
| `Passed` | All assertions held. |
| `Failed { reason }` | An assertion failed. `reason` is a structured payload, not a string. |
| `Timeout` | Watchdog fired before the runtime returned. |
| `OutOfMemory` | Heap cap reached and the fixture did not declare `expect.oom = true`. |
| `CapabilityDenied { capability }` | A guarded operation was denied and the fixture did not declare it. |
| `Skipped { reason }` | `requires` declared a feature the build does not have. |
| `Crash { reason }` | Runtime panicked or aborted. **Must be rare; CI hard-fail.** |

A fixture that declares `expect.timeout` and times out within the
declared duration produces `Passed`, not `Timeout`. The same goes for
`expect.oom = true` + `OutOfMemory`, and for `expect.throws = "..."`
+ a matching diagnostic.

## 5. Output modes

### 5.1 Default human output

- One line per test: `<status>  <duration_ms>ms  <name>`.
- Final summary block:

  ```
  passed  : 100
  failed  : 0
  timeout : 0
  oom     : 0
  cap     : 0
  skipped : 1
  crash   : 0
  duration: 1.532s
  ```

- Human output is **not stable**. CI must not parse it.

### 5.2 `--json` (NDJSON)

`--json` emits NDJSON: one JSON object per test, then one final
`summary` object.

Per-test schema:

```json
{
  "kind": "test",
  "name": "string concat loop is O(n)",
  "path": "tests/engine/strings/concat-loop.ts",
  "outcome": "Passed",
  "duration_ms": 12,
  "diagnostics": [],
  "stdout": "len=4000\n",
  "stderr": ""
}
```

`outcome` values:

- `"Passed"`
- `{ "Failed": { "reason": <structured> } }`
- `"Timeout"`
- `"OutOfMemory"`
- `{ "CapabilityDenied": { "capability": "fs_read" } }`
- `{ "Skipped": { "reason": "requires=ts" } }`
- `{ "Crash": { "reason": "<short message>" } }`

`diagnostics` is an array of the same `Diagnostic` JSON shape ADR-0003
defines for the public API, with `kind`, `code`, `message`, `span`,
`frames`, `cause`.

Final summary schema:

```json
{
  "kind": "summary",
  "schema_version": 1,
  "suite": "engine",
  "passed": 100,
  "failed": 0,
  "timeout": 0,
  "oom": 0,
  "capability_denied": 0,
  "skipped": 1,
  "crash": 0,
  "duration_ms": 1532
}
```

`schema_version` is bumped on incompatible changes.

### 5.3 `--json` is stable

Snapshot tests in the runner pin the per-test and summary shapes.
Adding a new optional field is **not** a bump. Renaming, removing, or
changing the type of an existing field bumps `schema_version`.

## 6. `--bless`

- Updates only golden artifacts (`*.expected.txt`,
  `*.expected.stderr.txt`, `*.expected.json`) for fixtures whose
  `bless` is `true`.
- Refuses to bless fixtures whose declared `expect` would still fail
  after update (e.g., `throws = "..."` with no thrown value).
- Refuses to create a new golden file when none exists unless
  `--bless --new-goldens` is passed.
- Prints a list of changed files and the diff size.
- Exits `0` if anything was written, `1` otherwise.

## 7. Diagnostics in tests

Tests assert against the **structured `Diagnostic`** the public API
returns, not against `Debug`-formatted strings.

- `expect.throws = "Kind::CODE"` matches `diagnostic.kind` and
  `diagnostic.code`. The runner does not match against `message` text
  to keep fixtures stable across copy edits.
- The `diagnostics` array in `--json` output is rendered via the same
  formatter the CLI uses, so a fixture's expected output mirrors what
  the user sees.

## 8. CI integration

- `otter test --suite engine --json` is the canonical CI invocation
  for engine fixtures. CI parses the NDJSON stream and exports
  `passed`, `failed`, `timeout`, `oom`, `crash` as metrics.
- A `Crash` count > 0 fails the CI job regardless of `passed/failed`
  ratios.
- `--suite smoke --json` is run on every CI job.
- `--suite test262 --json --filter <area>` is run when the touched
  feature area maps to a Test262 directory.
- The CI plumbing itself is added in a later task; this spec just
  names the contract.

## 9. Non-goals

- **Not** a user-facing test runner that competes with Jest / Vitest.
- **No** `describe` / `it` DSL during foundation.
- **No** parallel execution by default.
- **No** snapshot testing beyond the simple golden-file mechanism.
- **No** plugin system.
- **No** browser-mode shimming, no DOM, no `jsdom`.
- **No** TAP / JUnit / `xunit-xml` output during foundation. JSON
  is the one machine format.

## 10. Versioning and stability

- `schema_version: 1` is the initial wire format for `--json`.
- A spec amendment is required to bump `schema_version`, add an
  outcome variant, change exit-code semantics, or change the fixture
  metadata grammar in a non-additive way.
- Adding new optional metadata fields, new tags, or new optional
  golden file kinds is **not** a bump.

## Spec amendments

### 2026-04-29 — `_*` helper directory convention

- **Change:** The fixture discoverer skips every directory whose
  name starts with `_`. Files inside such a directory are not
  enumerated as standalone fixtures. They exist solely as module
  helpers loaded by sibling entry fixtures (e.g.,
  `tests/engine/modules/static-import/_modules/util.ts` is
  imported by `tests/engine/modules/static-import/entry.ts` but
  never run on its own).
- **Reason:** Module-graph fixtures inherently span more than one
  file. Running each helper as a standalone fixture would either
  fail the unrelated assertions or silently pass — neither is
  useful. The leading-`_` prefix is the common Rust / Cargo
  convention for "private to siblings, not a public entry point".
- **Linked task:** task 36a — modules / live bindings / dynamic
  import (closed; merged into the README's banner section).

### 2026-04-29 — `node_modules/` directories skipped

- **Change:** The discoverer also skips any directory whose name
  is exactly `node_modules` (case-sensitive). Files inside are
  npm packages loaded through the module-graph driver, never
  tests.
- **Reason:** task 36b ships fixtures with real `node_modules/`
  trees on disk to exercise `oxc_resolver`-driven bare-specifier
  resolution. Without this skip the `index.js` files inside the
  packages would be picked up as standalone fixtures and either
  fail or produce noise.
- **Linked task:** [task 36b — oxc_resolver loader upgrade](
  ../tasks/36b-modules-npm-and-workspace-resolution.md).

When a slice changes the harness contract, append a dated entry of
the form:

```markdown
### 20YY-MM-DD — <short title>

- **Change:** <what was added / removed / changed>
- **Reason:** <why>
- **Linked task:** [task XX](../tasks/XX-...)
```

## References

- ADR-0003 (public API & CLI): [`../adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
- Bytecode dump / trace spec: [`./bytecode-dump-disasm-trace.md`](./bytecode-dump-disasm-trace.md)
- Foundation plan §M11.
- Task: [`../tasks/05-spec-otter-test-harness.md`](../tasks/05-spec-otter-test-harness.md)
