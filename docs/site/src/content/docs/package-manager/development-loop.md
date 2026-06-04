---
title: "Package Manager Development Loop"
---

Otter's package-manager loop is intentionally flat:

```sh
otter init -y
otter add left-pad
otter install
otter check app.ts
otter run app.ts
otter run --bin fixture-tool
otter test
otter outdated
```

`otter run` is the single execution command for files, package scripts, and
local package binaries. There is no separate `otter exec` command and no
first-party `otter build` command in this phase.

## Commands

| Command | Behavior |
|---|---|
| `otter init [-y]` | Create `package.json` with Otter defaults. |
| `otter install` | Resolve the project or import an existing npm/pnpm lockfile, fetch registry metadata and tarballs, materialize `node_modules`, link package bins, run install lifecycle hooks, and write `otter.lock`. |
| `otter add <pkg[@range]>` | Mutate the selected manifest dependency bucket, then run the install flow. |
| `otter remove <pkg>` | Remove the package from dependency buckets, refresh `otter.lock`, and prune removed registry packages and bin links. |
| `otter outdated` | Read the manifest, lockfile, and registry metadata, then print a semver-aware outdated table. It does not mutate `package.json`, `otter.lock`, or `node_modules`. |
| `otter run <target>` | Resolve a path first, then a package script, then a local package binary. |
| `otter check <file>` | Compile through the same module resolver and package graph as `run`, without executing user code. |
| `otter test` | Run the test harness through the same runtime session and package graph as `run`. |

Package-manager commands are explicit first-party CLI operations. Registry
network access, project/cache filesystem writes, and install lifecycle
subprocesses are not gated by runtime capability flags. Runtime APIs, dynamic
imports, and hosted APIs remain capability-gated.

## Lockfile Migration

Otter's native lockfile is `otter.lock`. It is the only lockfile format Otter
writes.

For migration, Otter accepts existing npm and pnpm lockfiles when `otter.lock`
is not present:

1. `pnpm-lock.yaml`
2. `npm-shrinkwrap.json`
3. `package-lock.json`

Those files are normalized into the runtime package graph in memory. This lets
commands such as `otter run`, `otter check`, `otter test`, `otter remove`, and
`otter outdated` work against already materialized `node_modules` during a
migration. `otter install` also consumes the foreign lockfile, materializes the
recorded tarballs, records package metadata from extracted manifests, runs
available install lifecycle hooks, and writes a native `otter.lock`.

## Lifecycle Hooks

Otter runs package lifecycle scripts during explicit package-manager installs.
The install hook subset follows pnpm's dependency install path:

1. `preinstall`
2. `install`
3. `postinstall`

Ordinary package scripts such as `build`, `test`, or `prepare` are not treated
as install lifecycle hooks. For extracted package tarballs, Otter also records
and runs pnpm/npm-compatible implicit `install = "node-gyp rebuild"` when a
package has `binding.gyp` and no explicit `preinstall` or `install` script.
Lifecycle scripts run with the package root as the working directory and
project-local `node_modules/.bin` on `PATH`.

## Outdated

`otter outdated` compares three versions:

- `Current`: installed version from `otter.lock`.
- `Wanted`: newest registry version satisfying the manifest range.
- `Latest`: registry `latest` dist-tag, falling back to the highest semver
  version when the tag is missing.

The table includes a `Bump` column:

| Bump | Meaning |
|---|---|
| `patch` | `Latest` changes only the patch number. |
| `minor` | `Latest` changes the minor number within the same major. |
| `major` | `Latest` changes the major number. |
| `unknown` | One side is not valid semver or no semver delta exists. |

By default `outdated` checks `dependencies`. Use `--dev`, `--peer`, or
`--optional` to include the other manifest buckets.

```sh
otter outdated --dev --optional
```

`outdated` exits with status `1` when at least one dependency is outdated and
`0` when everything checked is current.

## Resolver Contract

When a package graph is available, it is authoritative for bare package
imports from graph-contained packages:

- undeclared bare package imports are rejected even when a matching directory
  exists under `node_modules`;
- package self-reference by package name is allowed without a dependency edge;
- optional and peer dependencies have distinct diagnostics when missing;
- peer dependencies may resolve to an already installed package with the same
  name when the peer range edge points at an unmaterialized placeholder;
- package `exports`, package `imports`, `main`, `module`, package `type`, and
  dependency edge kinds are carried into the runtime through lightweight DTOs.

`otter-runtime` does not depend on `otter-pm`; product crates adapt the richer
package-manager graph into the runtime DTO.
