# Otter New Engine — Task Index

This directory decomposes
[`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md) into a
sequence of self-contained task files. Each task is a small, executable unit of
work with explicit scope, acceptance criteria, verification commands, and a
pointer to the next task.

## Working rules

1. **Read the plan first.** Every task assumes the foundation plan and
   `AGENTS.md` are read. Tasks reference the plan's milestones (M0–M12) but do
   not duplicate the rationale.
2. **No broad VM implementation until ADR/spec tasks are accepted.** Tasks
   `01`–`06` are documentation-only. They set the rules; later tasks follow
   them.
3. **Vertical slice policy.** Implementation tasks (from `09` onwards) follow
   the slice order:
   `parser/frontend → compiler → bytecode → interpreter → public API → CLI →
   otter test fixture → benchmark (if hot path)`.
4. **Old code is not deleted in feature commits.** Cleanup is a separate task
   stream (see `01-repo-cleanup-map.md` and follow-up cleanup tasks). The
   legacy `crates/*` directories stay on disk as frozen reference. They are
   **not** built, tested, linted, or modified during foundation, and **no
   code is migrated out of them** — the new engine in `crates-next/*` is
   written from scratch (ADR-0001 §8). A single end-of-foundation cleanup
   commit deletes them outright.
5. **Staging directory.** New foundation crates live in `crates-next/`,
   chosen and locked by task `02` / ADR-0001. There is no "promotion move":
   `crates-next/` is where the new engine stays. The end-of-foundation
   cleanup commit removes the legacy `crates/*` directories and the
   `[workspace] exclude = ["crates/*"]` entry.
6. **OXC owns parsing.** No regex-parsing of JS/TS, no hand-rolled lexer or
   parser anywhere on the new path. See ADR-0003.
7. **Interpreter-only foundation.** No JIT work in any foundation task. JIT
   metadata may be preserved on bytecode, but execution and all benchmarks are
   interpreter-only.
8. **TypeScript first.** Every implementation slice that has an end-to-end
   surface accepts both `.js` and `.ts` input. `.ts` is not deferred.
9. **Status updates.** Each task file has a `## Status` section. After the
   task is executed, update it with: what was done, what commands were run,
   pass/fail, deltas vs. baseline, and any follow-ups.
10. **Edits use `apply_patch`.** Hand-edit changes go through `apply_patch`,
    not raw `cat`/`echo` rewrites. Check `git status` before edits and never
    touch unrelated unstaged work.
11. **LLM-friendly module docstrings (mandatory).** Every Rust file in
    every `crates-next/*` crate begins with a `//!` module docstring
    that lists `# Contents`, optional `# Invariants`, optional
    `# See also`. This rule is locked by ADR-0001 §6 and enforced by
    a CI grep check from task `07` onward.

## Task list

Documentation / guardrails (M0):

| #  | File | Type | Status |
|----|------|------|--------|
| 01 | [01-repo-cleanup-map.md](./01-repo-cleanup-map.md) | cleanup map | **done** (2026-04-26) |
| 02 | [02-staging-directory-decision.md](./02-staging-directory-decision.md) | ADR | **done** (2026-04-26) |
| 03 | [03-adr-oxc-frontend.md](./03-adr-oxc-frontend.md) | ADR | **done** (2026-04-26) |
| 04 | [04-adr-public-api-cli-shape.md](./04-adr-public-api-cli-shape.md) | ADR | **done** (2026-04-26) |
| 05 | [05-spec-otter-test-harness.md](./05-spec-otter-test-harness.md) | spec | **done** (2026-04-26) |
| 06 | [06-spec-bytecode-dump-disasm-trace.md](./06-spec-bytecode-dump-disasm-trace.md) | spec | **done** (2026-04-26) |

Implementation slices (M1–M7), strictly in this order:

| #  | File | Slice | Status |
|----|------|-------|--------|
| 07 | [07-vm-harness-minimal-interpreter.md](./07-vm-harness-minimal-interpreter.md) | VM harness skeleton | **done** (2026-04-26) |
| 08 | [08-typescript-frontend-skeleton.md](./08-typescript-frontend-skeleton.md) | TS frontend skeleton | not started |
| 09 | [09-string-core-slice.md](./09-string-core-slice.md) | String core | not started |
| 10 | [10-string-methods-slice.md](./10-string-methods-slice.md) | String methods | not started |
| 11 | [11-number-core-slice.md](./11-number-core-slice.md) | Number core | not started |
| 12 | [12-boolean-nullish-control-flow-slice.md](./12-boolean-nullish-control-flow-slice.md) | Booleans / control flow | not started |
| 13 | [13-calls-frames-slice.md](./13-calls-frames-slice.md) | Calls and frames | not started |

Tasks for objects/shapes, arrays, builtins, and conformance ratchets are
**intentionally not in this batch**. They are added once tasks `01`–`13` are
landed and the foundation is honest about what it can hold up.

## Related documents

- Foundation plan: [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
- Agent rules: [`AGENTS.md`](../../../AGENTS.md)
- ADRs (created by tasks `02`–`04`): [`../adr/`](../adr/)
- Specs (created by tasks `05`–`06`): [`../specs/`](../specs/)

## Definition of done for this batch

- All 13 task files exist, are self-contained, and reference their next task.
- Tasks `01`–`06` have written outputs (cleanup map, ADRs, specs) under
  `docs/new-engine/`.
- No runtime code under `crates-next/` is implemented yet beyond what each
  slice task explicitly requires.
- Legacy `crates/*` directories are not built or modified by any foundation
  command (ADR-0001).
