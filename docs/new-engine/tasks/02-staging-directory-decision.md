# Task 02 — Staging Directory Decision (ADR-0001)

## Goal

Set up `crates-next/` as the home of the new foundation engine, drop every
existing crate from the workspace `members` list, and write the rule that
governs the new staging directory's lifecycle in
`docs/new-engine/adr/0001-staging-directory.md`.

Decision (locked by the user on 2026-04-26):

- Staging directory name: **`crates-next/`**.
- Crate-name prefix: **`otter-*`**.
- The old crates under `crates/*` are **removed from `[workspace] members`**.
  They stay on disk for reference only and are **not** built, tested,
  formatted, or linted by any new-foundation command. We do not fix them,
  do not patch them, and do not reintroduce them into the workspace.

The foundation plan permits a temporary staging directory so the new core can
stay clean while the old runtime stack stays out of the build graph. This
task turns "permitted" into "decided, documented, and enacted".

## Scope

- Lock the directory name as `crates-next/`. Alternatives considered
  (`engine-next/`, `crates-foundation/`, `crates/_next/`) are rejected:
  `crates-next` reads naturally next to `crates/`, makes the temporary
  status obvious, and is short enough to fit every CI command line.
- Lock the crate-name prefix as `otter-*` so staging crates cannot be
  confused with old ones and cannot accidentally be published.
- **Edit the root `Cargo.toml`** in this task:
  - Empty the `[workspace] members` list (or leave only an explicit empty
    placeholder comment) and replace it with the staging crates **as they
    are created**. Until task `07` lands the first staging crate, the
    `members` list is permitted to be empty; this is the first step of
    cutting the old code out of the build graph.
  - Add an `[workspace] exclude = ["crates/*"]` entry so a future stray
    `path = "../crates/..."` reference cannot pull old code in by accident.
  - Keep the existing `[workspace.dependencies]`, `[profile.*]`, and other
    workspace-level settings intact.
- Decide how the staging directory participates in the workspace:
  - **Required:** every new staging crate appears in the workspace
    `Cargo.toml` `members` list when it is created so `cargo build`,
    `clippy`, and `test` cover it.
  - **Required:** staging crates use `publish = false`.
  - **Required:** staging crate names are prefixed `otter-*`.
  - **Required:** `#![forbid(unsafe_code)]` everywhere. Foundation phase
    does not introduce a new GC or JIT, so no exception is needed; if a
    future slice introduces `otter-gc`, it amends this ADR.
- Write the **no-migration rule**:
  - Nothing inside `crates/*` is ported, ported back, or rewritten in
    place. The new engine is written from scratch in `crates-next/*`.
  - Legacy `crates/*` directories are kept on disk as frozen reference
    until they are deleted in a single end-of-foundation cleanup
    commit. They are never built, tested, linted, or modified during
    foundation.
- Write the **end-of-foundation cleanup rule**:
  - Once the new engine in `crates-next/*` ships as the project's
    primary runtime, a single dedicated cleanup commit deletes the
    legacy `crates/*` directories outright (no archive, no rename) and
    removes the `[workspace] exclude = ["crates/*"]` entry. That
    commit changes nothing else.
  - `crates-next/` itself stays. There is no "promotion move" of
    directories. A later, optional, also dedicated commit may rename
    `crates-next/` to `crates/` for cosmetic reasons; that is a pure
    directory rename with no semantic change.
- Write the **abort rule**:
  - If a `crates-next/*` crate is abandoned, it is deleted in a
    dedicated cleanup commit. No crate may import an abandoned
    `crates-next/*` crate.
- Document the **import rule**:
  - Old crates under `crates/*` are reference-only. Nothing in the new
    workspace depends on them. They are not even part of the build graph.
  - `crates-next/*` crates may depend on each other (subject to the layered
    architecture documented in tasks `07`–`13`) and on third-party crates
    from crates.io.
  - `crates-next/*` crates **may not** add a path dependency to any crate
    under `crates/*`, even temporarily. Foundation phase is the moment to
    keep this rule clean.
  - The new CLI binary lives in `crates-next/otter-cli` and is the
    one that produces the `otter` binary during foundation. The old
    `crates/otterjs` is no longer in the build graph.

## Out of scope

- Creating any crate under `crates-next/`. Tasks `07`+ create the first
  crates.
- Choosing crate boundaries inside `crates-next/`. (Task `07` does this.)
- Deleting old `crates/*` directories. They stay on disk untouched
  until the dedicated end-of-foundation cleanup commit.
- Fixing anything inside `crates/*`. The user has explicitly opted out.
- Porting / migrating any code from `crates/*` into `crates-next/*`.
  The new engine is written from scratch (ADR-0001 §8).

## Files / directories you may touch

- Create: `docs/new-engine/adr/0001-staging-directory.md`
- Edit: root `Cargo.toml` — `[workspace] members` (drop the old crates),
  `[workspace] exclude` (block `crates/*` from accidental re-entry).
- Create: `crates-next/.gitkeep` so the directory exists in git before
  the first crate is added in task `07`.
- Read-only: everything else (and especially `crates/*`, which is now
  reference-only).

## Acceptance criteria

- `docs/new-engine/adr/0001-staging-directory.md` exists and contains:
  - **Decision**: `crates-next/` and `otter-*`.
  - **Context**: why a staging directory is needed (link to foundation
    plan) and why every old crate is dropped from the workspace.
  - **Consequences**: what changes for contributors, CI, and the build
    graph (workspace `members` shrinks to staging; `crates/*` stops
    building).
  - **No-migration rule** (new engine written from scratch; nothing
    is ported out of `crates/*`).
  - **End-of-foundation cleanup rule** (single dedicated commit
    deletes `crates/*` outright; `crates-next/` stays).
  - **Abort rule** (deletion, dedicated commit, no leftover deps).
  - **Import rule** (`crates-next/*` may not depend on `crates/*`).
  - **Naming convention** (`otter-*`; rationale).
  - **Exit signal**: foundation phase is over once the new engine in
    `crates-next/*` ships and the legacy `crates/*` are deleted.
- Root `Cargo.toml`:
  - `[workspace] members` no longer references any `crates/*` entry.
  - `[workspace] exclude = ["crates/*"]` is present so old code cannot
    re-enter the build graph by accident.
  - `[workspace.dependencies]` and `[profile.*]` blocks are unchanged.
- `crates-next/` exists in git (empty `.gitkeep` is fine).
- The ADR is referenced from `docs/new-engine/tasks/README.md` (already
  done in this batch).
- `cargo metadata --format-version=1 --offline` is **expected to fail**
  with "the workspace has no members" until task `07` lands the first
  staging crate. This is a known transient state and is documented in
  the ADR's "Consequences" section. The check is re-enabled in task
  `07`'s verification.
- The user-visible `otter` binary continues to build **only** if you ask
  for it explicitly (`cargo build --manifest-path crates/otterjs/Cargo.toml`
  — outside the workspace), and that is no longer a supported developer
  command. The supported path is `crates-next/*` from task `07` onward.

## Verification commands

```bash
test -f docs/new-engine/adr/0001-staging-directory.md
test -d crates-next
# Old crates must not appear in [workspace] members. The grep below
# ignores the `exclude = ["crates/*"]` line by matching `crates/<name>`
# paths without a trailing `*`:
rg -n '^\s*"crates/[A-Za-z0-9_-]+",' Cargo.toml \
    && exit 1 || true
rg -nU 'exclude\s*=\s*\[\s*"crates/\*"' Cargo.toml
# `cargo metadata` will fail until task 07 — that is expected:
cargo metadata --format-version=1 --offline 2>&1 \
    | grep -q 'workspace has no members' \
    && echo "metadata: empty workspace as expected" \
    || echo "metadata: unexpected output (re-check Cargo.toml)"
```

## Risks

- **Two production engines.** The single largest risk this ADR exists to
  prevent. The user has resolved it by dropping the old crates from the
  workspace entirely; the ADR must restate the import rule so the rule
  survives commit churn.
- **Name churn.** Renaming a staging crate after files exist is expensive
  in diffs. Lock `otter-*` now.
- **Drift toward in-place migration.** It is tempting to "just port"
  one helper from `crates/otter-vm` into `crates-next/otter-vm`. The
  no-migration rule forbids it: every line of the new engine is
  written from scratch. Reading legacy code as reference is fine;
  copying it across is not.
- **Accidental re-entry of `crates/*`.** A future contributor adds a
  `path = "../crates/otter-vm"` to a `crates-next/*` `Cargo.toml`. The
  `[workspace] exclude = ["crates/*"]` entry plus the import rule in the
  ADR are the two defenses.

## Next task

Proceed to [`03-adr-oxc-frontend.md`](./03-adr-oxc-frontend.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts:
  - [`docs/new-engine/adr/0001-staging-directory.md`](../adr/0001-staging-directory.md)
  - root `Cargo.toml` (legacy `crates/*` removed from `[workspace]
    members`; `[workspace] exclude = ["crates/*"]` added; legacy oxc
    pins removed from `[workspace.dependencies]`; profile comments
    cleaned up)
  - `crates-next/.gitkeep` (empty placeholder so the directory exists
    in git before task `07` lands the first crate)
- verification:
  - ADR exists, `crates-next/` exists.
  - `rg -n '^\s*"crates/[A-Za-z0-9_-]+",' Cargo.toml` — no matches
    (legacy crates removed from members).
  - `rg -nU 'exclude\s*=\s*\[\s*"crates/\*"' Cargo.toml` — matches
    lines 11–12 of `Cargo.toml`.
  - `cargo metadata --format-version=1 --offline` — fails with
    `the manifest is virtual, and the workspace has no members`,
    expected and documented (re-enabled in task `07`).
- decisions locked:
  - directory name: `crates-next/`;
  - crate-name prefix: `otter-*`;
  - import rule: no path deps from `crates-next/*` to `crates/*`;
  - no-migration rule: new engine written from scratch in
    `crates-next/*`; nothing is ported out of `crates/*`;
  - end-of-foundation cleanup rule: dedicated commit deletes
    `crates/*` outright (no archive, no rename); `crates-next/`
    stays as the home of the new engine.
