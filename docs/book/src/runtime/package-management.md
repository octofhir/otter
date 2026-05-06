# Package Management

The parked package-manager crates are kept because Otter will need package,
workspace, and type installation flows again:

- `crates/otter-pm`
- `crates/otter-pm-manifest`
- `crates/otter-pm-lockfile`

The intended package-manager shape is one in-memory dependency graph with
adapters for existing lockfile formats. Otter should drop into real projects
without forcing a lockfile migration.

Lockfile precedence:

1. `otter-lock.yaml` when no foreign lockfile exists.
2. `pnpm-lock.yaml`
3. `bun.lock`
4. `yarn.lock`
5. `npm-shrinkwrap.json`
6. `package-lock.json`

Write back to the detected lockfile format. Do not create a second lockfile next
to an existing project lockfile.

Workspace discovery should support:

- `pnpm-workspace.yaml`;
- `package.json#workspaces` in array and object forms;
- pnpm-style selectors such as `pkg...`, `...pkg`, `./path`, `[origin/main]`,
  and `!exclude`.

The active runtime must not depend on these parked crates until the package
manager is ported onto the active stack intentionally.
