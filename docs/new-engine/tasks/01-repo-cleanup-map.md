# Task 01 — Repository Cleanup Map

## Goal

Produce one authoritative document — `docs/new-engine/repository-map.md` —
that classifies every workspace member, every top-level docs/script/asset, and
every checked-in result/fixture directory into one of four buckets:

- **active** — required by the new engine path, public API, CLI, conformance
  runner, or current CI;
- **parked** — temporarily retained old code with a named reason and an exit
  condition; not imported by active crates; receives no new features;
- **reference-only** — kept as a frozen reference for porting work; readable,
  not buildable as a runtime path;
- **delete candidate** — slated for removal in a dedicated cleanup commit.

This task **classifies**. It does **not** delete anything. Deletes happen in
follow-up cleanup tasks once classifications are reviewed.

## Scope

- Walk every entry under:
  - `crates/`
  - `docs/`
  - `benchmarks/`
  - `scripts/`
  - `examples/`
  - `packages/`
  - `release/`
  - any top-level `*.md` plan/note
  - any top-level `test262_*`, `*results*`, `*-baseline*`, `*-dump*`, or
    profiler output directory
- For each entry, record:
  - path
  - bucket (active / parked / reference-only / delete candidate)
  - one-line reason
  - if `parked`: the named exit condition (which slice replaces it)
  - if `delete candidate`: whether any active crate currently imports it
    (must be `no` before deletion is allowed)
- Map active dependency edges into parked crates. Every such edge is a defect
  to track in a follow-up cleanup task.
- Identify generated / local-only artifact paths that should be moved to
  `.gitignore` instead of committed.

## Out of scope

- Actually deleting files. (Follow-up cleanup tasks do this.)
- Refactoring active crates.
- Touching `crates-next/` (it does not exist yet — see task `02`).
- Editing `ES_CONFORMANCE.md` content (only confirm whether it exists).

## Files / directories you may touch

- Create: `docs/new-engine/repository-map.md`
- Read-only: everything else

You **must not** modify, move, or delete any other file as part of this task.

## Acceptance criteria

- `docs/new-engine/repository-map.md` exists and lists every workspace member
  declared in the root `Cargo.toml` plus every top-level docs/scripts/results
  directory.
- Every entry has a bucket, a one-line reason, and (where applicable) an exit
  condition.
- Every `delete candidate` entry has an explicit `imported by active crates:
  yes/no` line.
- Active → parked dependency edges are listed in a dedicated section with the
  importing crate, the parked crate, and the call site (file path, no line
  numbers required).
- A short `## Open questions` section lists anything that needs a human
  decision before a deletion task can be scheduled.
- No code, doc, or asset outside `docs/new-engine/repository-map.md` is
  modified by this task.

## Verification commands

```bash
# Workspace members enumerated:
rg '^\s*"crates/' Cargo.toml

# Verify the map mentions every workspace member:
for c in $(rg -No '"crates/[^"]+"' Cargo.toml | tr -d '"'); do
  rg -q "$c" docs/new-engine/repository-map.md \
    || echo "MISSING: $c"
done

# Workspace still builds (no behavior change expected):
cargo metadata --format-version=1 --offline >/dev/null
```

## Risks

- **Over-classifying as `delete`.** When unsure, prefer `parked` with an
  explicit exit condition. Delete buckets must be defensible.
- **Hidden active edges.** A crate marked `parked` but transitively imported
  by an active crate is a build-graph defect. Mark it as such; do not "fix"
  it in this task.
- **Drift from `Cargo.toml`.** The map is hand-written. A follow-up CI check
  (planned in task `12-…` after this batch) should fail when workspace
  membership and the map diverge — out of scope here.

## Next task

Proceed to [`02-staging-directory-decision.md`](./02-staging-directory-decision.md)
once the cleanup map is reviewed and merged.

## Status

- **done**
- last update: 2026-04-26
- artifacts: [`docs/new-engine/repository-map.md`](../repository-map.md)
- verification:
  - `rg '^\s*"crates/' Cargo.toml` — lists 13 workspace members.
  - coverage loop (every `crates/<name>` from `Cargo.toml` is mentioned
    in the map) — `DONE`, no `MISSING:` lines.
  - `cargo metadata --format-version=1 --offline >/dev/null` — `OK`.
- what was done:
  - classified every workspace member into active / parked /
    reference-only / delete-candidate buckets;
  - documented the active → parked dependency edge
    (`crates/otterjs` → `crates/otter-nodejs` at
    `crates/otterjs/Cargo.toml:28`, call sites in
    `crates/otterjs/src/main.rs`);
  - flagged `crates/otter-macros` and `crates/otter-profiler` as
    active code that is **not** declared in the workspace `members`
    list (build-graph defect; logged as Open Question 1);
  - listed generated/local artifacts that should be `.gitignore`d;
  - listed every other top-level entry (`docs/`, `benchmarks/`,
    `scripts/`, `examples/`, `packages/`, `release/`, `tests/`,
    `test262_results/`, `scratch/`, `definitely-typed-pr/`, root
    docs and shell scripts).
- follow-ups (revised after task `02` / ADR-0001):
  - **Voided by ADR-0001** (legacy `crates/*` is reference-only and
    out of the build graph; no fixing required):
    - ~~task-01b — add `otter-macros` / `otter-profiler` to workspace
      `members`~~ — not relevant; legacy crates excluded.
    - ~~task-01e — remove `otterjs → otter-nodejs` edge~~ —
      not relevant; legacy crates excluded.
  - **Still scheduled** (delete / archive of generated and
    superseded files; not blocked by anything):
    - task-01c — deletion commit for `test262_results/`,
      `benchmarks/results/`, `benchmarks/node_modules/`,
      `benchmarks/c2-strings-latest.log`, `scratch/`,
      `run-benchmark.sh`, `run-bun-benchmark.sh`, `test-server.sh`;
      extend `.gitignore`.
    - task-01d — archive move for `PRODUCTION_READINESS_PLAN.md`,
      `TOOLING_ROADMAP.md`, `gc_migration_baseline.md`, `ROADMAP.md`.
  - **Reframed** (after ADR-0001 the legacy stack is reference-only,
    so per-leaf classification of legacy benchmarks/examples no longer
    blocks the foundation work; do them only if a foundation slice
    needs the historical data):
    - task-01f — subdivide `benchmarks/{async,io,http,sql,jit,memory,
      startup,src}/`.
    - task-01g — classify each `examples/*jit*` file.
  - **Foundation prerequisite** (still required by foundation plan
    §M0; will be authored as a dedicated task before the conformance
    ratchet starts):
    - task-01a — recreate `ES_CONFORMANCE.md`.
