# Otter PM redesign: multi-format lockfile + monorepo support

## Context

Current [crates/otter-pm/](../crates/otter-pm/) is a clean MVP but is a dead end for
real-world projects:

- writes a single custom format (`otter.lock` JSON + `otter.lockb` binary), ignores
  any existing `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock` — so a
  user trying Otter inside a pnpm repo silently creates a divergent lockfile;
- zero workspace support — no `pnpm-workspace.yaml`, no `"workspaces"` field,
  no inter-package linking;
- resolver is a flat BFS against `registry.npmjs.org` with no peer deps,
  overrides, catalogs, optional filtering, or conflict handling;
- everything lives in one crate, which will not scale as these features land.

Goal: Otter drops into existing JS projects without forcing the team to switch
PMs. `otter install` in a pnpm/npm/yarn/bun project reads that project's
lockfile, produces a bit-identical layout, and writes back to the same file —
with workspaces behaving like pnpm's `--filter` universe.

## Design overview

**One in-memory graph type, many on-disk adapters.** `otter-pm-lockfile::LockfileGraph`
is the canonical representation, with `parse()` / `write()` trampolines that
dispatch on detected `LockfileKind`. Single most important architectural
decision — every other subsystem (resolver, linker, install, drift detection)
talks to `LockfileGraph`, never to a format-specific shape.

**Format precedence:**
1. `otter-lock.yaml` (ours) — only when no other lockfile exists
2. `pnpm-lock.yaml`
3. `bun.lock` (text; reject `bun.lockb` with actionable error)
4. `yarn.lock` (peek for `__metadata:` → classic vs berry)
5. `npm-shrinkwrap.json`
6. `package-lock.json`

**Write-back rule:** detect on read, keep on write — never surprise a user with
a second lockfile next to theirs. Our own format is used *only* when none of
the others is present.

**Workspace discovery:** `pnpm-workspace.yaml` → `package.json#workspaces`
(array **and** `{packages: [...]}` object forms), glob-expanded with the
existing `glob` crate. Pnpm-style `--filter` selectors (`pkg...`, `...pkg`,
`./path`, `[origin/main]`, `!exclude`) so a monorepo user can run
`otter install --filter @app/web...` and see only the relevant subgraph
installed. This is the shape pnpm / Turborepo users already have in muscle
memory.

**Virtual store + symlinks (pnpm-style layout) as default; hoisted as opt-in.**
Global CAS at `~/.cache/otter-pm-store/`, per-project `node_modules/.otter/`
virtual store, top-level `node_modules/<name>` symlinks. `nodeLinker: hoisted`
escape hatch for legacy toolchains. Same layout means tools like Next.js that
rely on it don't need special-casing.

## Crate split

Replace the monolithic [crates/otter-pm/](../crates/otter-pm/) with a workspace
of small crates, one responsibility each:

| New crate | Purpose | Key types |
|---|---|---|
| `otter-pm-manifest` | `package.json` + `pnpm-workspace.yaml` parse/serialize | `PackageJson`, `WorkspaceConfig`, `Workspaces` enum (array / object) |
| `otter-pm-lockfile` | Canonical graph + format adapters (npm/pnpm/yarn/bun/shrinkwrap/ours) | `LockfileGraph`, `LockedPackage`, `DirectDep`, `DepType`, `LocalSource`, `LockfileKind`, `DriftStatus` |
| `otter-pm-workspace` | Monorepo discovery + `--filter` selector engine | `find_workspace_packages`, `Selector`, `EffectiveFilter`, `select_workspace_packages` |
| `otter-pm-registry` | npm registry HTTP client + `.npmrc` config + packument cache | `RegistryClient`, `Packument`, `NetworkMode` |
| `otter-pm-store` | Global CAS (blake3 file hashes, sha512 tarball integrity), cache dirs | `Store`, `PackageIndex`, `StoredFile` |
| `otter-pm-resolver` | Peer-context pass, semver resolution, optional/platform filtering, overrides | `Resolver`, `ResolvedPackage`, `PeerContextOptions`, `DependencyPolicy` |
| `otter-pm-linker` | `node_modules` materialization (isolated + hoisted) | `Linker`, `NodeLinker` enum, `HoistedPlacements` |
| `otter-pm-scripts` | `package.json#scripts` runner with lifecycle hooks, PATH, `approve-builds` policy | `ScriptRunner`, `ScriptPolicy` |
| `otter-pm` (thin orchestrator) | Top-level `install` / `add` / `remove` / `run` flows, calls into the crates above | `Installer` |

[crates/otterjs/src/main.rs](../crates/otterjs/src/main.rs) keeps its current
subcommands (`install`, `add`, `remove`, `exec`, `init`) and gains `run`,
`import` (foreign → otter lockfile conversion), plus `-r`/`--filter` /
`--filter-prod` globals.

## LockfileGraph: the one type everyone talks to

Core fields in `otter-pm-lockfile::LockfileGraph`:

```
importers:   BTreeMap<String, Vec<DirectDep>>         // workspace paths → direct deps
packages:    BTreeMap<String, LockedPackage>           // dep_path → resolved pkg
settings:    LockfileSettings                          // auto-install-peers, etc.
overrides:   BTreeMap<String, String>                  // pnpm overrides block
catalogs:    BTreeMap<String, BTreeMap<String, CatalogEntry>>
times, skipped_optional_dependencies, ignored_optional_dependencies, ...
```

`DirectDep.specifier: Option<String>` records the user's `package.json`
range *only* for formats that preserve it (pnpm v9). Drift detection is a
string compare. For npm/yarn/bun we short-circuit drift to `Fresh` since the
format has no specifier to compare against.

`LocalSource` covers `file:`, `link:`, `git+…`, and remote tarball deps,
with a `dep_path()` hashing scheme (sha256 → first 8 hex) so filesystem keys
stay cross-platform stable.

`LockfileKind` enum + `detect_existing_lockfile_kind(project_dir)` gives us
the read-detect, write-preserve loop. Writer dispatch:

```rust
match kind {
    LockfileKind::Otter | LockfileKind::Pnpm => pnpm::write(path, graph, manifest)?,
    LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::write(...)?,
    LockfileKind::Yarn => yarn::write_classic(...)?,
    LockfileKind::YarnBerry => yarn::write_berry(...)?,
    LockfileKind::Bun => bun::write(...)?,
}
```

Every format adapter is a leaf module — parse takes a `&Path`, returns
`LockfileGraph`; write takes `(&Path, &LockfileGraph, &PackageJson)` and
produces bytes. No adapter ever imports another; they share only the
canonical graph.

## Build sequence

Land one crate pair at a time; each pair is self-contained and testable.

**Phase 1 — foundation (no behavior change for end users):**
1. `otter-pm-manifest`: lift [install.rs#PackageJson](../crates/otter-pm/src/install.rs)
   into its own crate, add `WorkspaceConfig` + `Workspaces` enum. Unit-test against
   fixtures; add pnpm workspace yaml, npm array form, yarn object form.
2. `otter-pm-lockfile` skeleton: `LockfileGraph`, `LockedPackage`,
   `DirectDep`, `LocalSource`, `LockfileKind`, `detect_existing_lockfile_kind`.
   First adapter: `otter` format (straight port of the existing
   [lockfile.rs](../crates/otter-pm/src/lockfile.rs) mapped onto the new shape).

**Phase 2 — format adapters (the main value prop):**
3. `pnpm.rs` adapter — read/write `pnpm-lock.yaml` v9. Single biggest win.
   Preserve `settings:`, `overrides:`, `time:`, `catalogs:`,
   `ignoredOptionalDependencies:` round-trip.
4. `npm.rs` adapter — `package-lock.json` + `npm-shrinkwrap.json`.
5. `yarn.rs` adapter — classic (v1 text) + berry (v2+ yaml). Dispatch by
   peeking for `__metadata:`.
6. `bun.rs` adapter — text `bun.lock`. Reject binary `bun.lockb` with
   actionable error.
7. Wire `detect_existing_lockfile_kind` + `parse_lockfile_with_kind` +
   `write_lockfile_preserving_existing` into the existing `Installer`.
   End-to-end test: `cargo test -p otter-pm-lockfile` + a matrix fixture
   under `tests/fixtures/lockfile_interop/{pnpm,npm,yarn,bun}/`.

**Phase 3 — workspaces:**
8. `otter-pm-workspace`: `find_workspace_packages` (yaml precedence, then
   `package.json#workspaces`), `WorkspacePkg`, `Selector`.
9. Global CLI flags in [crates/otterjs/src/main.rs](../crates/otterjs/src/main.rs):
   `-r/--recursive`, `-F/--filter <sel>`, `--filter-prod <sel>`.
   `EffectiveFilter` threaded into every mutating command.
10. `install` / `add` / `remove` become workspace-aware: iterate kept
    importers, resolve each, write one unified lockfile with per-importer
    entries.
11. `workspace:*` / `workspace:^` / `workspace:~` protocol in resolver —
    point at the sibling package's on-disk version, not the registry.

**Phase 4 — resolver / linker polish:**
12. Lift [resolver.rs](../crates/otter-pm/src/resolver.rs) into
    `otter-pm-resolver`; add peer-context pass (`apply_peer_contexts`),
    optional platform filtering (`os` / `cpu` / `libc`), `overrides` support,
    catalog resolution (`catalog:` protocol).
13. `otter-pm-linker`: lift [content_store.rs](../crates/otter-pm/src/content_store.rs)
    + [install.rs](../crates/otter-pm/src/install.rs) file-copy paths; add
    `NodeLinker::{Isolated, Hoisted}` + `.otter/` virtual-store layout +
    bin symlinks. Global CAS default at `~/.cache/otter-pm-store/v1/files/`.

**Phase 5 — QoL:**
14. `otter import` — parse any foreign lockfile, write `otter-lock.yaml`.
    Trivial once Phase 2 is done (just call `parse_for_import` + `write_lockfile_as`).
15. `otter run <script>` — expose [scripts.rs](../crates/otter-pm/src/scripts.rs)
    as a subcommand. Lifecycle hooks (`preinstall`, `postinstall`) with
    `approve-builds` policy.
16. `otter dedupe`, `otter why`, `otter outdated` — all operate on
    `LockfileGraph`, low marginal cost once the graph exists.

## Files to create / modify

**Create** (new crate layout under [crates/](../crates/)):
- `crates/otter-pm-manifest/{Cargo.toml,src/lib.rs,src/workspace.rs}`
- `crates/otter-pm-lockfile/{Cargo.toml,src/{lib,otter,pnpm,npm,yarn,bun,dep_path_filename,graph_hash,merge}.rs}`
- `crates/otter-pm-workspace/{Cargo.toml,src/{lib,selector}.rs}`
- `crates/otter-pm-registry/{Cargo.toml,src/{lib,client,config}.rs}` (lift from `registry.rs` + `manifest_cache.rs`)
- `crates/otter-pm-store/{Cargo.toml,src/{lib,dirs}.rs}` (lift from `content_store.rs`)
- `crates/otter-pm-resolver/{Cargo.toml,src/{lib,peer_context,platform,override_rule}.rs}`
- `crates/otter-pm-linker/{Cargo.toml,src/{lib,hoisted,sys}.rs}`
- `crates/otter-pm-scripts/{Cargo.toml,src/{lib,policy}.rs}`

**Modify:**
- [crates/otter-pm/src/lib.rs](../crates/otter-pm/src/lib.rs): re-export shims pointing at the new crates, keep until call sites move, then delete.
- [crates/otterjs/src/main.rs](../crates/otterjs/src/main.rs): add `-r`/`--filter`/`--filter-prod`; wire `install`/`add`/`remove` through the new orchestrator.
- [crates/otterjs/src/commands/install.rs](../crates/otterjs/src/commands/install.rs), [add.rs](../crates/otterjs/src/commands/add.rs), [remove.rs](../crates/otterjs/src/commands/remove.rs): switch to the new `Installer` API (takes `EffectiveFilter` + `project_root`).
- Workspace `Cargo.toml`: register the new member crates.

**Reuse (do not rewrite from scratch):**
- [crates/otter-pm/src/content_store.rs](../crates/otter-pm/src/content_store.rs) — already has macOS clonefile + hardlink paths; port into `otter-pm-store` with minor tweaks (blake3 for per-file hash, sha512 kept for tarball integrity).
- [crates/otter-pm/src/registry.rs](../crates/otter-pm/src/registry.rs) + [manifest_cache.rs](../crates/otter-pm/src/manifest_cache.rs) — ETag cache, retry logic, parallel packument fetch. Port into `otter-pm-registry`.
- [crates/otter-pm/src/scripts.rs](../crates/otter-pm/src/scripts.rs) — lifecycle hooks, PATH, fuzzy matching. Port into `otter-pm-scripts`.

## Verification

**Per-crate unit tests** (run `cargo test -p otter-pm-<crate>` for each new
crate):
- `otter-pm-manifest`: round-trip pnpm-workspace.yaml, `workspaces`
  array + object forms, catalogs / overrides extraction.
- `otter-pm-lockfile`: for each adapter, a fixture under
  `tests/fixtures/lockfile_interop/<fmt>/` containing a real lockfile
  from a small public project; parse → `LockfileGraph` → write → assert
  stable bytes (or at minimum: parse → re-parse the write → identical
  graph).
- `otter-pm-workspace`: glob match, path match, `!` exclude, `foo...`
  dependency walk, `[origin/main]` changed-since git.

**Integration tests** under [tests/](../tests/):
- `install_pnpm_project.rs` — scaffold a pnpm fixture (workspace + 2
  packages + real `pnpm-lock.yaml`), run `otter install`, assert (a)
  `node_modules/` exists with the expected symlink layout, (b) the
  *same* `pnpm-lock.yaml` is what's on disk (no `otter-lock.yaml`
  appears).
- `install_npm_project.rs` — same for npm.
- `install_workspace_filter.rs` — `--filter @app/web...` only installs
  the selected subgraph.
- `install_fresh_no_lockfile.rs` — assert `otter-lock.yaml` is created
  only when no other lockfile is present.

**Manual smoke (golden path):**
```
cd /tmp && rm -rf smoke-pnpm && git clone --depth 1 <url> smoke-pnpm
cd smoke-pnpm && cargo run -p otterjs -- install
# Assert: no new lockfile, pnpm-lock.yaml untouched in git diff, node_modules installs cleanly
node --experimental-vm-modules node_modules/.bin/<some-bin>
```

**Non-goals (explicitly out of scope):**
- Publishing parity with `npm publish`. Land after the install path is stable.
- Git / tarball dep protocols beyond the happy path — `LocalSource`
  variants are modeled in the graph but resolver support can stub with
  `todo!("git deps — see phase 6")` until someone needs it.
- Telemetry, audit, `doctor` commands.
- Keeping `otter.lockb` binary format. One on-disk format is enough;
  the multi-adapter design makes the custom binary format worthless as
  a perf lever.

## Current progress

**Phase 1 — done.** `otter-pm-manifest` + `otter-pm-lockfile` landed with
the `otter` adapter (YAML + legacy JSON round-trip) + format-kind
detection + Phase-2 stubs for the other four formats. 27 tests green.

**Phase 2 — partial.** `pnpm` + `npm` adapters landed (11 tests green).
`bun` + `yarn` adapters started in this session but reverted — see
`crates/otter-pm-lockfile/src/{bun,yarn}.rs`, currently 18-line and
73-line stubs respectively. Re-apply the full implementations next.
