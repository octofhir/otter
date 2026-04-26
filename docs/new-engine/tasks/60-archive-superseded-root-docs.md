# Task 60 — Archive superseded root-level docs

## Goal

Move pre-foundation planning docs into `docs/archive/` so the root
shows only living plans.

## Scope

- Move into `docs/archive/`:
  - `PRODUCTION_READINESS_PLAN.md`
  - `TOOLING_ROADMAP.md`
  - `ROADMAP.md`
  - `gc_migration_baseline.md`
- Each archived file gets a single-line header at the top:
  `> Superseded — see [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../NEW_ENGINE_FOUNDATION_PLAN.md).`
- The file content is otherwise unchanged.
- Update `docs/new-engine/repository-map.md` row entries that
  reference these files.

## Out of scope

- Touching `crates/*` (legacy crates stay).
- Editing the foundation plan itself.

## Files / directories you may touch

- Root-level superseded plan files.
- `docs/archive/` (new).
- `docs/new-engine/repository-map.md` (link updates).

## Acceptance criteria

- The four files no longer exist at the repo root.
- `docs/archive/` contains them with the superseded-by header.
- `docs/new-engine/repository-map.md` references the new paths.

## Verification commands

```bash
ls docs/archive/
ls -la PRODUCTION_READINESS_PLAN.md 2>&1 | grep "No such file"
```

## Risks

- Tooling that links to the root paths breaks; grep the repo for
  references and update them.

## Status

- not started
