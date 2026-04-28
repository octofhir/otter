# Task 54 — wire `expect.*` into the engine harness

## Goal

Extend `crates-next/otter-test/src/lib.rs` so the runner reads
the assertion fields the harness spec already documents in
[`docs/new-engine/specs/otter-test-harness.md`](../specs/otter-test-harness.md)
§2.1, instead of only checking `expect.exit_code`. This unblocks
shorter, more direct fixtures — current foundation tests rely on
the `function fail() { return undefined.x; }` trick because the
runner cannot inspect the script's completion value, captured
stdout, or the structured `Diagnostic` shape.

## Scope

- Implement these `expect.*` fields:
  - `expect.value` — JSON-compare against the script's
    completion value (the value `Otter::run_*` returns). Use the
    runtime's `Value` → JSON conversion (the same path the public
    API exposes; do not invent a new serializer).
  - `expect.stdout` — exact-match against captured stdout.
  - `expect.stdout_contains` — array of substrings that must all
    appear in captured stdout.
  - `expect.stderr_contains` — same for stderr.
  - `expect.throws = "Kind::CODE"` — match
    `Diagnostic.kind` and `Diagnostic.code` from `OtterError`'s
    structured payload.
- Capture the runtime's stdout / stderr through a sink hooked
  into the public API. (`console.log` doesn't exist yet — when
  it lands, it routes here.)
- Update `ExpectBlock` and `parse_metadata` to deserialize the
  new fields, then add an explicit assertion pass that walks
  every set field and accumulates a structured failure reason.
- Add fixtures that exercise each field (one per assertion shape)
  under `tests/engine/harness-self/`. These also serve as the
  golden behaviour for the runner.

## Out of scope

- `expect.timeout` — keep deferred until per-test watchdogs land
  (tracked in §3 of the spec, not in this slice).
- `expect.oom` — same; needs the public API to surface
  `OutOfMemory` distinctly first.
- `console.log` implementation. This task wires the **sink**;
  the global lands later. A fixture using `expect.stdout` works
  the moment `console.log` is implemented, not before.
- `--bless` plumbing (golden-file rewrite). Spec §6.

## Files / directories you may touch

- `crates-next/otter-test/`
- `crates-next/otter-runtime/` (only to expose a stdout / stderr
  sink hook on the public API if one is not already there).
- `tests/engine/harness-self/`

## Acceptance criteria

- A fixture with `expect.value = "7"` passes when the script
  evaluates to `7` and fails with a structured reason otherwise.
- A fixture with `expect.throws = "Compile::TS_UNSUPPORTED"`
  passes for a TS enum, fails for a working script.
- Engine suite remains green.
- The runner's NDJSON output gains no new top-level fields
  (the assertion expansion is internal — outcome shape is
  unchanged).

## Risks

- Capturing stdout means changing the public API surface (or
  adding a hidden, embedder-facing sink). Choose the smallest
  change that keeps ADR-0003 stable; if a public additive change
  is required, amend the ADR in the same slice.
- `expect.value` JSON comparison has to handle `undefined`
  (which has no JSON encoding); decide on a sentinel
  (`null`? a private marker string?) and document it inline.

## Status

- not started
