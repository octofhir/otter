# Otter Repository Map (Foundation Phase)

This is the authoritative classification of every workspace member,
top-level directory, and notable file in the Otter repository at the
start of the new-engine foundation phase. Each entry is assigned to one
of four buckets:

- **active** — required by the current runtime stack, public API, CLI,
  conformance runner, or current CI; must keep building and passing
  tests during foundation.
- **parked** — temporarily retained code with a named exit condition.
  Must not be imported by active crates. No new features land here.
- **reference-only** — kept frozen for porting work; readable but not
  buildable as a runtime path.
- **delete candidate** — slated for removal in a dedicated cleanup
  commit. Each entry records whether any active crate currently
  imports it (`imported by active crates: yes/no`).

Generated artifacts and local-only output are also called out below.

The map is authored to match
[`NEW_ENGINE_FOUNDATION_PLAN.md`](../../NEW_ENGINE_FOUNDATION_PLAN.md)
§Repository Cleanup Policy. It does **not** delete anything — it sets
up follow-up cleanup tasks.

## How to read this document

Every entry is one row:

```
<path> — <bucket> — <one-line reason>
exit:    <only for "parked"; the slice that retires it>
imports: <only for "delete candidate"; yes/no>
```

When the path is a workspace crate the row is followed by a sub-row that
states the `Cargo.toml` `members` membership.

## Workspace crates

After ADR-0001 (task `02`), the workspace `[workspace] members` list
is empty until task `07` lands the first staging crate. The legacy
crates under `crates/*` are excluded from the build graph by
`[workspace] exclude = ["crates/*"]` and are not part of the new
engine. Per ADR-0001 §8, **no code is migrated out of `crates/*`** —
the new engine in `crates-next/*` is written from scratch.

Every `crates/*` directory below is therefore **reference-only**: it
stays on disk so engineers can read it while writing the new engine,
but it is not built, tested, linted, or modified during foundation.
A single end-of-foundation cleanup commit (ADR-0001 §9) deletes all
of them outright.

The reason rows below describe what each legacy crate **was** for so a
reader can decide quickly whether to open it for reference.

### `crates/otter-gc`

- bucket: **reference-only**
- previous role: garbage collector for the legacy runtime stack.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-vm`

- bucket: **reference-only**
- previous role: legacy VM, interpreter, value/object model, intrinsics.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-runtime`

- bucket: **reference-only**
- previous role: legacy public runtime/embedding surface.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otterjs`

- bucket: **reference-only**
- previous role: legacy `otter` CLI binary.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-jit`

- bucket: **reference-only**
- previous role: legacy JIT pipeline. Foundation phase is interpreter-
  only (foundation plan §15); no JIT crate is even planned for the new
  engine in this phase.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-modules`

- bucket: **reference-only**
- previous role: legacy hosted modules (`otter:kv`, `otter:sql`,
  `otter:ffi`).
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-web`

- bucket: **reference-only**
- previous role: legacy Web API surfaces (`TextEncoder`/`Decoder`,
  …).
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-macros`

- bucket: **reference-only**
- previous role: descriptor-driven proc-macros for the legacy stack.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-profiler`

- bucket: **reference-only**
- previous role: CPU profiler / sampler for the legacy stack.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-pm`

- bucket: **reference-only**
- previous role: legacy package manager + bundled `.d.ts` types.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-pm-manifest`

- bucket: **reference-only**
- previous role: manifest reader/writer for the legacy package manager.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-pm-lockfile`

- bucket: **reference-only**
- previous role: lockfile reader/writer for the legacy package manager.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-test262`

- bucket: **reference-only**
- previous role: legacy full-corpus Test262 runner. The new engine
  brings its own conformance harness (foundation plan §M11; spec in
  `docs/new-engine/specs/otter-test-harness.md`).
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-nodejs`

- bucket: **reference-only**
- previous role: legacy Node.js compatibility shim.
- exit: deleted in the end-of-foundation cleanup commit.

### `crates/otter-node-compat`

- bucket: **reference-only**
- previous role: parallel legacy Node-compat experiment.
- exit: deleted in the end-of-foundation cleanup commit.

## Top-level files

### `README.md`

- bucket: **active**
- reason: project entry point. Will be rewritten once the new engine
  in `crates-next/*` ships as the project's primary runtime.

### `LICENSE`

- bucket: **active**
- reason: required.

### `AGENTS.md`, `CLAUDE.md`

- bucket: **active**
- reason: agent / contributor guidance. Updated incrementally as the
  foundation plan changes the project shape.

### `NEW_ENGINE_FOUNDATION_PLAN.md`

- bucket: **active**
- reason: the foundation plan. Currently untracked in git
  (`?? NEW_ENGINE_FOUNDATION_PLAN.md`); commit in a dedicated commit
  alongside this map.

### `ROADMAP.md`

- bucket: **reference-only**
- reason: pre-foundation roadmap; superseded by
  `NEW_ENGINE_FOUNDATION_PLAN.md` for any decisions that conflict.
- exit (planned): when the foundation phase is done, replace with a
  short link to the new roadmap.

### `PRODUCTION_READINESS_PLAN.md`

- bucket: **reference-only**
- reason: large pre-foundation plan; useful historical context but
  conflicting with the foundation plan's milestones. Move to
  `docs/archive/` in a dedicated cleanup task with a "superseded-by"
  pointer at the top.

### `TOOLING_ROADMAP.md`

- bucket: **reference-only**
- reason: pre-foundation tooling notes. Same treatment — move to
  `docs/archive/` with a pointer once the foundation reaches M2.

### `gc_migration_baseline.md`

- bucket: **reference-only**
- reason: GC migration baseline numbers. Useful for comparison once a
  GC slice opens; otherwise archive.

### `Cargo.toml`, `Cargo.lock`

- bucket: **active**
- reason: workspace manifest and lock.

### `Justfile`

- bucket: **active**
- reason: developer shortcuts (`just fmt`, `just test262-filter`, …).

### `cliff.toml`, `release-plz.toml`, `Dockerfile`, `.dockerignore`,
  `.gitmodules`, `.gitignore`

- bucket: **active**
- reason: release / containerization / VCS plumbing.

### `node_compat_config.toml`

- bucket: **parked**
- reason: tied to the parked Node compat surfaces.
- exit: removed with the parked Node compat crates.

### `test262_config.toml`

- bucket: **active**
- reason: configuration for `crates/otter-test262`.

### `otter-logo.png`

- bucket: **active**
- reason: branding asset.

### `run-benchmark.sh`, `run-bun-benchmark.sh`, `test-server.sh`

- bucket: **delete candidate**
- reason: single-purpose ad-hoc scripts at the repo root that duplicate
  or predate `benchmarks/`. Foundation plan §Repository Cleanup Policy
  forbids "random scripts at the root".
- imports: no (none of `crates/*` references them).

## Top-level directories

### `crates/`

- bucket: **active**
- reason: workspace crates listed above.

### `docs/`

- bucket: **active**
- reason: project docs. New-foundation docs live under
  `docs/new-engine/`. Existing files inside `docs/` are evaluated below.
- subentries:
  - `docs/alloc-hotspot-fix-plan.md` — **reference-only**
    (pre-foundation perf plan). Archive when foundation reaches M3.
  - `docs/bytecode-v2.md` — **reference-only**
    (predates the bytecode dump/disasm spec from task `06`). Archive
    once the new bytecode spec is merged.
  - `docs/deployment.md` — **active** (deployment guide).
  - `docs/gc-migration-plan.md` — **reference-only**
    (predates foundation). Archive at M2.
  - `docs/pm-redesign.md` — **active** (current `otter-pm` design).
  - `docs/new-engine/` — **active** (new foundation tasks/ADRs/specs).

### `benchmarks/`

- bucket: **reference-only** (with delete-candidate sub-entries for
  generated artifacts; see below).
- reason: every script under `benchmarks/` targets the legacy `otter`
  binary. The new engine has its own Criterion benchmarks living
  inside the staging crates (e.g.,
  `crates-next/otter-vm/benches/strings.rs`) and its own `tests/engine/`
  fixtures. The legacy `benchmarks/` directory is not exercised by
  foundation work. If a workload there is interesting (e.g.,
  `benchmarks/cpu/json.ts`), the relevant slice task may copy the
  **input file** (not Rust harness code) into the staging crate's
  bench input set; the bench harness itself is rewritten from scratch.
- delete-candidate sub-entries (committed generated output, deleted in
  task `01c`):
  - `benchmarks/results/` — committed benchmark output dumps.
  - `benchmarks/node_modules/` — vendored npm modules, should be
    `.gitignore`d.
  - `benchmarks/c2-strings-latest.log` — committed log.

### `scripts/`

- bucket: **reference-only**
- reason: every script is wired to the legacy `otter` binary or to
  the legacy Test262 runner. The new engine in `crates-next/*` does
  not use them. They stay on disk until the end-of-foundation cleanup
  commit. If a foundation task needs equivalent functionality, it
  re-implements it as a fresh script under `scripts/` (or under the
  staging crate it belongs to) without copying from the legacy file.

### `examples/`

- bucket: **reference-only**
- reason: every example illustrates the legacy `otter` CLI. They are
  not exercised by the new engine in `crates-next/*`, which has its
  own fixture suite under `tests/engine/` (tasks `07`–`13`). Deleted
  in the end-of-foundation cleanup, possibly with selected examples
  ported to `tests/engine/` as fixtures along the way (case by case,
  in their respective slice tasks).

### `packages/`

- bucket: **reference-only**
- reason: `packages/otter-types/` is the legacy `@types/otter` publish
  artifact derived from `crates/otter-pm/`. The new engine will
  generate its own `.d.ts` artifacts when the new package-management
  surface lands (post-foundation). Until then, the directory is not
  used.

### `release/`

- bucket: **reference-only**
- reason: holds `macos-jit-entitlements.plist`. Tied to the legacy
  JIT-shipping `otter` binary. Foundation phase is interpreter-only,
  so this is not used by the new engine. Deleted in the
  end-of-foundation cleanup if no new slice revives JIT signing.

### `tests/`

- bucket: **active**
- reason: the foundation `otter test` fixtures live under
  `tests/engine/` (created starting with task `07`). The Test262
  corpus reference at `tests/test262/` (git submodule) stays as the
  upstream conformance corpus that the curated `--suite test262`
  fixtures consume — that submodule is **not** rebuilt and is **not**
  legacy code.
- subentries:
  - `tests/test262/` — **active** (Test262 corpus submodule).
  - `tests/engine/` — **active** (created by tasks `07`–`13`).

### `test262_results/`

- bucket: **delete candidate**
- reason: 60+ committed JSONL result dumps (`task-*`, `c1-*`, `c2-*`,
  `final*`, etc.). Foundation plan §Repository Cleanup Policy
  explicitly calls out "old test result directories" for deletion and
  CI should fail on committed result artifacts. The directory should
  also be added to `.gitignore` once emptied.
- imports: no (no `crates/*` references this directory).

### `scratch/`

- bucket: **delete candidate**
- reason: single file (`check_sizes.rs`) — a one-off debug script.
  Foundation plan §Repository Cleanup Policy: "one-off debug scripts"
  are deletion targets. The directory itself should be `.gitignore`d
  if scratch space is desired.
- imports: no.

### `definitely-typed-pr/`

- bucket: **reference-only**
- reason: vendored `DefinitelyTyped` working tree for a previous PR.
  Not imported by any crate.
- exit (planned): delete in a dedicated cleanup commit once we confirm
  no in-flight PR depends on it.

### `target/`

- bucket: **generated** (already `.gitignore`d).

### `.github/`, `.idea/`, `.vscode/`, `.claude/`

- bucket: **active**
- reason: standard repo / editor / agent config. (`.idea`, `.vscode`,
  `.claude` are local editor state but already in repo; leave as-is.)

## Legacy intra-`crates/*` dependency edges (informational)

After ADR-0001, every `crates/*` crate is reference-only and out of
the workspace build graph, so legacy dependency edges between them
are no longer defects — nothing builds them. They are listed below for
readers who open the legacy code as reference.

| Legacy importer | Legacy imported | Legacy `Cargo.toml` entry |
|-----------------|------------------|---------------------------|
| `crates/otterjs` | `crates/otter-nodejs` | `crates/otterjs/Cargo.toml:28` |

Other intra-`crates/*` edges exist (e.g., `otter-runtime` →
`otter-vm` → `otter-gc`) but are not enumerated here; the legacy
crates' own `Cargo.toml` files are the source of truth and they are
not touched by foundation work.

## Generated artifacts that should be `.gitignore`d (not committed)

- `test262_results/` (already noted as delete candidate; once empty,
  add to `.gitignore`).
- `benchmarks/results/` (delete candidate; add to `.gitignore`).
- `benchmarks/node_modules/` (delete candidate; should never be
  committed).
- `benchmarks/c2-strings-latest.log` and similar `*.log` files.
- `*.cpuprofile`, `*.heapsnapshot`, `*.trace.json`, `*.folded`,
  `timeout-dump*.txt` anywhere outside approved fixture directories
  (foundation plan §M12 mentions a CI check for this).
- `scratch/` if kept as a developer scratch space.

The actual `.gitignore` edit is a follow-up cleanup task, not part of
this map.

## Open questions (need a human decision)

After ADR-0001 the legacy `crates/*` is reference-only, so the
build-graph defects identified earlier (e.g.,
`otter-macros`/`otter-profiler` not in workspace `members`,
`otterjs → otter-nodejs` edge) are no longer relevant — they belong
to legacy code that nobody builds. The remaining open questions are:

1. **`PRODUCTION_READINESS_PLAN.md`** is large (≈82 KB). Confirm
   nothing in the new-engine plan supersedes it silently before any
   archive move.

2. **`run-benchmark.sh`, `run-bun-benchmark.sh`, `test-server.sh`**
   at the repo root — confirm they are unused before deletion.

3. **`ES_CONFORMANCE.md` is missing.** AGENTS.md and the foundation
   plan §M0 require it. Schedule task-`01a` to recreate it once the
   new engine has an early conformance signal (probably during the
   first slice that runs Test262 fixtures, ≥ task `09`).

4. **`benchmarks/{async,io,http,sql,jit,memory,startup,src}/`** —
   classify each leaf as kept-for-foundation-bench-suite or
   delete-candidate only if a foundation slice needs the historical
   data. Otherwise leave the directory alone until the
   end-of-foundation cleanup commit.

## Follow-up tasks this map implies

- **task-01a**: recreate `ES_CONFORMANCE.md` once the new engine has
  enough surface to register a baseline (foundation plan §M0 / §M10).
- **task-01c**: dedicated deletion commit for `test262_results/`,
  `benchmarks/results/`, `benchmarks/node_modules/`,
  `benchmarks/c2-strings-latest.log`, `scratch/`, `run-benchmark.sh`,
  `run-bun-benchmark.sh`, `test-server.sh`. Update `.gitignore` to
  keep them out.
- **task-01d**: archive move for `PRODUCTION_READINESS_PLAN.md`,
  `TOOLING_ROADMAP.md`, `gc_migration_baseline.md`, `ROADMAP.md` into
  `docs/archive/` with `superseded-by` headers.
- **end-of-foundation cleanup** (ADR-0001 §9): single dedicated
  commit deletes every `crates/*` directory plus the
  `[workspace] exclude = ["crates/*"]` entry in `Cargo.toml`. Not a
  task `01x`; happens at the end of foundation.

These are not implemented in this task — they are scheduled in a
follow-up batch.

## Status

- created: 2026-04-26
- last updated: 2026-04-26
- coverage check: every workspace `members` entry plus
  `crates/otter-macros` and `crates/otter-profiler` is classified
  above; every top-level entry shown by `ls` is classified.
