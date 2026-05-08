# Engine Refactor Execution Plan

This is the active execution plan for aggressive engine, compiler, runtime, and
package-manager work. It intentionally allows breaking changes. Preserve the
active crate stack and remove weak abstractions instead of maintaining backward
compatibility shims.

Active stack:

```text
otter-gc -> otter-vm -> otter-runtime -> product crates
```

Active workspace today: `otter-bytecode`, `otter-syntax`, `otter-compiler`,
`otter-gc`, `otter-macros`, `otter-vm`, `otter-runtime`, `otter-test`,
`otter-test262`, `otter-cli` (binary `otter`), `otter-modules`, `otter-web`.

Out of scope here:

- `crates-legacy/*` (parked: `otter-nodejs`, `otter-node-compat`, plus parked
  package-manager experiments). No active build-graph dependency on these.
- JIT work in any tier. The interpreter is the foundation target.
- Third-party JS engine dependencies.
- Parallel runtime stacks under any new directory.
- First-party `otter build` / bundler work. Production readiness in this plan
  means install, resolve, run files, run package scripts, run package binaries,
  check, test, diagnostics, and profiling. Output bundling can come later after
  the package graph and compiler/runtime contract are stable.

This plan does not duplicate `RUNTIME_PUBLIC_API_PLAN.md`. Where a P0 item there
is already merged (hosted module surface, Web API bootstrap, `otter-modules`
and `otter-web` off direct `otter-vm` deps, dependency-direction test), this
plan extends rather than re-files.

## Operating Rules

- Ship vertical slices: parser/resolver input -> compiler -> bytecode -> VM ->
  runtime API -> CLI -> tests -> diagnostics.
- Package management is P0, not a later tooling add-on.
- Remove obsolete compatibility layers when a replacement lands.
- Keep one canonical representation for each runtime concept.
- Use `oxc` AST APIs for JS/TS analysis. Never regex-parse JS/TS.
- Reuse `oxc_resolver` (already in workspace deps) for bare-specifier and
  `package.json#exports` resolution; do not write a parallel resolver.
- New public APIs expose runtime concepts, not VM internals.
- Tests and diagnostics are part of every slice.
- Capability gating (`fs_read`, `net`, `subprocess`, …) applies to runtime
  execution, user code, dynamic imports, hosted APIs, and future lifecycle
  script execution. First-party package-manager commands (`install`, `add`,
  `remove`, `outdated`, `init`) are trusted CLI operations and do not require
  capability flags.
- The P0 developer loop is: `otter init`, `otter install`, `otter add/remove`,
  `otter outdated`, `otter run` / direct file shorthand, `otter check`,
  `otter test`, readable diagnostics, trace/profiling flags. `run` is the
  single user-facing execution command for files, package scripts, and local
  package binaries.

## Target Architecture

### VM Core

- One value model in `crates/otter-vm`.
- One object model with ordered observable property storage.
- Descriptor-first builtins: static specs install constructors, prototypes,
  methods, accessors, constants, and namespaces through one bootstrap path.
- Native functions carry explicit metadata: name, arity, constructor behavior,
  receiver policy, and diagnostics label.
- Function objects and arguments objects are first-class VM concepts, not
  special cases scattered through builtins.
- Runtime checkpoints are centralized for timeouts, interrupts, and OOM.

### Compiler Pipeline

- One source pipeline for `.js`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts`, `.cts`,
  `.json`, and future virtual modules.
- `ResolvedSource` (already present in `otter-runtime::module_loader`) is the
  boundary object. Extend it to carry: specifier, canonical `file://` URL or
  package URL, loader, module type, source bytes, source map reference, package
  scope (`PackageScope` from PM crate), and cache key.
- Compiler emits bytecode plus module metadata: imports, exports, live-binding
  slots, source spans, function metadata, and diagnostic anchors.
- TypeScript path: `.ts/.tsx/.mts/.cts` with type-syntax stripping/lowering;
  `tsconfig.json` `extends`, `baseUrl`, `paths`, and `compilerOptions.jsx`
  consulted at resolve/compile time. Diagnostics keep TS source spans.
- Bytecode is versioned, disassemblable, and snapshot-testable.
- Compiler diagnostics use stable error codes and source spans.

### Runtime Session

- `otter-runtime` owns the runtime session through the existing
  `Runtime`/`RuntimeBuilder` pair (`crates/otter-runtime/src/lib.rs`). Extend
  it — do not introduce a third top-level type — to own: module graph, module
  loader, job queue, source-map table, diagnostics sink, capability set,
  package-manager handle, and bootstrap policy.
- `otter-vm` must not know npm registry, package.json, lockfiles, filesystem
  traversal, network, or product APIs.
- Runtime bridges to VM through narrow hooks. Concrete hook split:
  - `resolve(specifier, referrer, kind) -> ResolvedTarget`
  - `load(target) -> ResolvedSource`
  - `compile(ResolvedSource) -> CompiledModule`
  - `enqueue_job(Job)`
  - `emit_diagnostic(Diagnostic)`
  - `check_capability(Capability, &CapabilityRequest)`
- Global installation is centralized through the existing `bootstrap.rs` path.
  No ad-hoc global mutation outside it.

### Package Manager

- Add active package-manager crates under `crates/*` with the canonical package
  names `otter-pm-manifest`, `otter-pm-lockfile`, and `otter-pm`. The old
  parked `crates-legacy/otter-pm*` crates have been deleted; PM reference code
  is no longer kept under `crates-legacy`.
- Package manager owns package.json parsing, dependency resolution (npm
  semver, workspace, file, tarball; git deferred), registry fetch, integrity
  verification, tarball extraction, content-addressed cache, lockfile,
  workspaces, bin linking, and lifecycle policy.
- Runtime module resolver consults the package manager through a read-only
  `PackageGraph` interface (package roots, exports, peer/optional flags). It
  must not mutate installs during VM execution unless an explicit
  command/session policy enables it.
- Package-manager install/update/remove commands do not gate normal registry
  network access, project/cache filesystem writes, or install lifecycle
  subprocesses through runtime capabilities. They are explicit CLI actions.
- Lifecycle scripts use the pnpm-style install subset (`preinstall`, `install`,
  `postinstall`) and are executed only by explicit package-manager install
  operations, not by runtime module loading.
- Lockfile filename: `otter.lock`. Format: TOML-compatible deterministic text
  using the workspace `toml` dependency. It must be diffable from the first
  slice.

## P0: Core Refactor And Package Manager

### P0.1 Runtime Session Boundary

- [x] Add a runtime-session section to `crates/otter-runtime` module-level docs
  that names the existing `Runtime`/`RuntimeBuilder` as the session owner.
- [x] Pull module-loader/module-graph state that currently lives on ad-hoc
  paths into `Runtime`-owned fields (loader, graph, source-map table,
  diagnostics sink, PM handle).
- [x] Define and land the runtime hook trait set: `resolve`, `load`, `compile`,
  `enqueue_job`, `emit_diagnostic`, `check_capability`. Compile-fail tests for
  non-`Send`/non-`Sync` misuse already exist
  (`crates/otter-vm/tests/compile_fail/`); extend them to cover the new hooks.
- [x] Extend the existing dependency-direction test
  (`crates/otter-runtime/tests/dependency_graph.rs`) to fail if any active
  product crate gains a direct `otter-vm` dependency.
- [x] Remove old bridge APIs (`module_api`, raw VM-shaped installers) once the
  new hooks compile through CLI execution. List the symbols deleted in the
  slice commit.
  - Removed bridge symbols are listed in `RUNTIME_PUBLIC_API_PLAN.md`:
    `otter_runtime::module_api`, `HostedNativeCall::from_raw`, and
    `GlobalClass::from_raw`.

Acceptance:

- `otter run file.ts` (via `cargo run -p otter-cli -- run file.ts`) goes
  through the runtime session, not direct VM glue.
- `otter-vm` has no package-manager, registry, filesystem-traversal, or
  product-API dependency (verified by `dependency_graph` test).
- All existing `otter-vm` and `otter-runtime` unit tests pass.
- The bridge symbols listed in `RUNTIME_PUBLIC_API_PLAN.md` "Removed bridges"
  stay removed; new ones do not appear.

### P0.2 Package Manager Foundation

- [x] Add `otter-pm-manifest`, `otter-pm-lockfile`, and `otter-pm` under
  `crates/*`; add them to `[workspace]` members and `[workspace.dependencies]`.
- [x] `otter-pm-manifest`: package.json + workspace manifest types, parse +
  validate, deterministic serialize. Include npm `workspaces` glob support and
  `pnpm-workspace.yaml` support in P0.
- [x] `otter-pm-lockfile`: deterministic `otter.lock` (TOML-compatible text)
  with stable key
  ordering. Roundtrip tests.
- [x] `otter-pm`: public `PackageGraph`, `PackageId`, `PackageRoot`,
  `PackageBin` types plus resolver/cache/install traits.
- [x] Add lockfile-only local project resolution for root, workspace, file,
  and registry-range dependency entries; record lifecycle metadata without
  executing scripts.
- [x] `otter-pm`: resolver (semver, workspace, file, tarball), content-
  addressed cache, registry client, install pipeline, lifecycle metadata
  policy.
- [x] Add CLI command foundations in `otter-cli`: interactive `otter init`
  (`--yes` for CI), `otter install`, `otter add`, `otter remove`, and
  `otter outdated` for semver-aware dependency freshness diagnostics. Mutating
  commands refresh manifests plus deterministic `otter.lock`; `outdated` is
  read-only. These explicit package-manager commands do not require runtime
  capability flags.
- [x] Define and wire initial `otter run <target> [args...]` resolution:
  existing path / `file://` entrypoint first, `package.json#scripts` second,
  local package binary third; script/bin ambiguity reports candidates plus
  `--script` / `--bin` disambiguation syntax.
- [x] Add npm registry metadata model plus deterministic filesystem metadata
  cache and pluggable registry client interface. Registry lockfile entries can
  be enriched with selected version, tarball URL, integrity string, lifecycle
  scripts, and bin metadata without downloading tarballs or executing scripts.
- [x] Add content-addressed tarball cache with SRI `sha512`/`sha256`
  verification, atomic writes, deterministic file-backed tarball client, and
  cache-first reuse path for enriched registry lockfile sources. Extraction is
  still deferred.
- [x] Add async-first metadata and tarball cache APIs on Tokio filesystem
  primitives so the install hot path does not block the async runtime. New PM
  filesystem/cache/client APIs are async-only; no sync compatibility surface is
  kept in the active package-manager crates.
- [x] Add CLI commands in `otter-cli`: `otter install`, `otter add`,
  `otter remove`, `otter outdated`, `otter init`, and one unified
  `otter run`. Update `ROADMAP.md` in the same slice so the flat command
  surface does not list a separate `exec` command.
- [x] Add read-only migration adapters for `pnpm-lock.yaml`,
  `npm-shrinkwrap.json`, and `package-lock.json`; normalize them into the
  native in-memory lock graph when `otter.lock` is absent. Otter still writes
  only `otter.lock`.
- [x] Make `otter install` consume a compatible npm/pnpm lockfile when
  `otter.lock` is absent, materialize the recorded tarballs, then write native
  `otter.lock` so migration is a one-command path.
- [x] Define `otter run <target> [args...]` resolution order: explicit path or
  URL entrypoint first, `package.json#scripts` second, local package binary
  third. Ambiguous targets must produce a diagnostic with the candidates and
  the disambiguation syntax.
- [x] Wire `otter check` through the same source/module resolver used by
  `otter run`; it may stay syntax/compile-only until type checking is
  re-enabled.
- [x] Wire `otter test` fixtures through the same runtime session and package
  graph so tests exercise the real development path.
- [x] Implement npm registry metadata fetch with on-disk cache + SHA integrity
  checks.
- [x] Implement tarball download/extract into the cache.
- [x] Implement dependency-graph resolution for npm semver, file, workspace,
  and tarball dependencies. Cycle and peer-dep behavior recorded in tests.
- [x] Implement workspace discovery from both package.json `workspaces` and
  `pnpm-workspace.yaml`.
- [x] Implement bin linking into a project-local `node_modules/.bin/`
  equivalent and a `PackageGraph::resolve_bin` lookup for `otter run`.
- [x] Record and execute pnpm-style install lifecycle hooks
  (`preinstall`, `install`, `postinstall`) during explicit package-manager
  install operations.

Acceptance:

- A fixture project at `tests/fixtures/pkg/<name>/` installs a small dependency
  tree from scratch and the second run is a no-op against the lockfile.
- Re-running install is lockfile-stable (byte-identical lockfile, no spurious
  write).
- `otter run <bin>` finds a package bin from the installed graph when no file
  or package script shadows it.
- Registry fetch and project/cache disk writes work by default for explicit
  package-manager commands.
- `otter check`, `otter test`, and `otter run` resolve installed package
  imports through the same package graph.
- Lifecycle scripts execute in P0 only from explicit package-manager install
  commands; runtime execution and module loading never trigger them.
- No active crate gains a path dependency on `crates-legacy/*`.

### P0.2b Development Loop Without bundling work

- [x] Define the production-ready non-bundler loop in CLI docs:
  `init -> install/add -> run/check/test -> diagnose/profile`.
- [x] Ensure direct file shorthand and `run` share the same runtime session.
- [x] Ensure `check` and `test` share the same module resolver and package
  graph as `run`.
- [x] Add fixture projects that exercise TS entry files, installed packages,
  workspace packages, JSON modules, and package bins.
- [x] Add diagnostics snapshots for missing package, blocked capability,
  syntax error, compile error, runtime throw, and package install failure.
- [x] Do not add `otter build` in P0/P1. Any future build/bundler work starts
  after package graph, source contract, module metadata, and sourcemaps are
  stable.

Acceptance:

- A project can be created, install dependencies, run a package script, execute
  a package bin, run a TS entrypoint, run a check-only pass, and run tests
  without a bundler.
- All failures in that loop produce stable pretty and JSON diagnostics.

### P0.3 Module Resolver And Package Graph

- [x] Build the runtime resolver on top of `oxc_resolver` (already a workspace
  dep and used in `crates/otter-runtime/src/module_loader.rs`); do not write a
  parallel resolver.
  - [x] Route filesystem relative specifiers, disk bare package lookup,
    conditional `exports`/`imports`, and disk package `module`/`main` fields
    through `oxc_resolver`; keep runtime-local code only for `file://`
    canonicalization and PM graph DTO gating/diagnostics.
- [x] Add a `PackageGraph` lookup hook so the resolver consults the PM
  `otter.lock` graph instead of walking only `node_modules` on disk.
  - [x] Enforce graph-gated bare package imports for graph-contained
    importers while allowing package self-reference by package name.
- [x] Implement package scope lookup from `package.json` (cached per
  directory).
  - [x] Add runtime-local package scope cache for graph-backed package roots
    with longest containing root wins.
- [x] Implement ESM/CJS package-type detection (`type` field, file extension).
  - [x] Use graph-backed nearest package `type` for ambiguous
    `.js`/`.ts`/`.jsx`/`.tsx` entry files while preserving hard
    `.mjs`/`.mts` and `.cjs`/`.cts` extension overrides.
- [x] Implement `exports`, `imports`, `main`, `module`, conditional resolution
  for the foundation condition set (`default`, `import`, `node`, `otter`).
  - [x] Disk package `exports` / `imports` / `main` / `module` resolution goes
    through `oxc_resolver` with ESM and CJS condition/main-field sets.
  - [x] Package `imports` foundation: exact `#alias` string mappings and
    condition-object mappings through `LoaderPackageRoot.imports`.
  - [x] Graph-backed package `exports` / `imports` pattern keys resolve through
    the same condition set and exact-key precedence.
- [x] Implement extension probing through the fixed ordered list already in
  `LoaderConfig`.
- [x] Implement JSON module loading (`with { type: 'json' }` plus
  `.json` extension behaviour).
- [x] Implement `tsconfig.json` `extends`, `baseUrl`, `paths`, plus
  `compilerOptions.jsx` reading at resolve time.
  - [x] Resolve `extends`, `baseUrl`, and `paths` through
    `oxc_resolver::TsconfigDiscovery::Auto` using importer-file-aware
    resolution.
  - [x] Read merged `compilerOptions.jsx` through `oxc_resolver` tsconfig
    discovery, including `extends`, when loading source for compilation.
- [x] Add resolver diagnostics with exact importer/specifier/condition context.

Acceptance:

- Local files, workspace packages, installed packages, and TS path-aliased
  specifiers all resolve through the same runtime path.
- Resolver tests cover: package scopes, conditional exports, missing packages,
  ambiguous extensions, `tsconfig` `paths`, and a JSON module.

### P0.4 Compiler/Bytecode Contract

- [x] Define and freeze the `ResolvedSource -> CompiledModule` contract on the
  `otter-compiler`/`otter-runtime` boundary.
- [x] Move source-span ownership into `CompiledModule` metadata. Runtime
  registers spans into a session-owned source-map table at compile time.
- [x] Add bytecode disassembly snapshots for every new opcode (extend the
  existing `otter-bytecode::disasm` snapshot tests).
- [x] Add import/export metadata emission (`imports`, `exports`,
  `live_binding_slots`).
- [x] Allocate the module record in runtime before evaluation begins.
- [x] Keep TypeScript execution as a first-class path: `.ts/.tsx/.mts/.cts`
  parses by default, type syntax is stripped/lowered while preserving spans.
- [x] Stabilize compile diagnostics with source URL, byte range, diagnostic
  code, and help text; CLI human output renders through OXC miette.

Acceptance:

- Every compiled module can dump bytecode plus import/export metadata via
  `otter run --dump-bytecode=json <file>`.
- Compile errors include source URL, range, stable diagnostic code, and help
  text.
- TS-only fixtures execute through `otter <file.ts>` without a TS-compiler
  preprocess step.

## P1: Object Model, Builtins, And Runtime Semantics

P1 sequencing: P1.1 (descriptor core) lands first, then P1.2 (function
semantics) builds on it, then P1.3 (builtin installation) consumes both.

### P1.1 Object/Descriptor Core

- [ ] Make property descriptors the canonical object mutation path.
- [ ] Ensure property order is deterministic and spec-observable
  (integer-indexed keys ascending, then strings in insertion order, then
  symbols in insertion order).
- [ ] Centralize `[[DefineOwnProperty]]`, `[[GetOwnProperty]]`, `[[Set]]`,
  `[[Delete]]`, and prototype-walk behavior in `crates/otter-vm/src/object.rs`.
- [ ] Add descriptor tests for data/accessor fields, configurability,
  enumerability, writability, symbol keys, integer-index ordering, and
  frozen/sealed/non-extensible interactions.

Acceptance:

- `just test262-filter "built-ins/Object/defineProperty"` and
  `built-ins/Object/getOwnPropertyDescriptor` do not regress.
- One internal property-mutation entry point per shape mutation kind; no
  bypass of descriptor enforcement remains.

### P1.2 Function And Arguments Semantics

- [ ] Finalize `Function` object layout and metadata
  (`crates/otter-vm/src/function_prototype.rs`,
  `native_function.rs`).
- [ ] Finalize native function metadata: name, length, constructability,
  receiver handling, and error label.
- [ ] Confirm unmapped arguments-object semantics
  (`crates/otter-vm/src/arguments_object.rs`).
- [ ] Confirm mapped arguments-object semantics under sloppy + simple-params.
- [ ] Add tests for `Function.prototype.call/apply/bind` and constructor
  behavior, including `bind` length/name composition and bound-target chain.

Acceptance:

- `just test262-filter "built-ins/Function"` and
  `language/arguments-object` do not regress.

### P1.3 Builtin Installation

- [ ] Convert remaining ECMAScript builtins to static descriptor specs through
  the existing builder/`bootstrap.rs` path.
- [ ] Centralize bootstrap order for globals, constructors, prototypes, and
  namespaces in one ordered list.
- [ ] Add snapshot tests for global property descriptors (key set, attributes,
  prototype identity).
- [ ] Add per-family Test262 baselines before changing semantics so deltas are
  attributable.

Acceptance:

- A single `bootstrap` snapshot pins the global object shape; changes to it
  are reviewed.
- No new `pub fn install_*` outside `bootstrap.rs` and the builder backend.

## P2: Runtime Jobs, Modules, And Diagnostics

### P2.1 Module Graph Evaluation

- [ ] Define module-record states: unresolved, resolved, compiled,
  instantiated, evaluating, evaluated, errored.
- [ ] Support ESM live bindings end-to-end (export-binding slot model in
  bytecode + runtime indirection).
- [ ] Support ESM cycles per HostLoadImportedModule semantics.
- [ ] Route dynamic `import()` through the same loader, gated through
  `check_capability`. Privileged remote/dynamic imports require explicit
  capabilities; entry-point + statically analyzable local graph stays the
  default-on path.
- [ ] Add CJS interop policy *after* ESM graph is stable. Documented as a
  follow-up; not a P2 acceptance gate.

Acceptance:

- A two-file ESM cycle (`a.ts <-> b.ts`) executes and observes spec live-
  binding behavior.
- Dynamic-import test denies a registry/HTTP specifier when the matching
  capability is absent and resolves it when present.

### P2.2 Job Queue And Async Boundary

- [ ] Model microtasks, timers, host jobs, and dynamic imports as
  runtime-owned queues on `Runtime`/`EventLoop`.
- [ ] Ensure promise settlement always hops through runtime job delivery.
- [ ] Add deterministic order tests for microtasks and timers.
- [ ] Keep JS/VM interaction on the runtime thread boundary; worker tasks may
  do plain Rust async, but VM/JS interaction hops back onto the scheduler.

Acceptance:

- Microtask + timer ordering test fixture passes deterministically.
- No `tokio::spawn` of work that touches `Interpreter`/`Value`/`Local` outside
  the runtime scheduler boundary.

### P2.3 Diagnostics

- [ ] Stabilize diagnostic types: load, resolve, parse, compile, permission,
  runtime, package-manager. Each has a stable error code.
- [ ] Add code-frame output and machine-readable JSON output (`--json`).
- [ ] Add stack-trace source mapping using the runtime-owned source-map table.
- [ ] Add error-object formatting tests (Error.cause, AggregateError).
- [ ] Add timeout-dump integration with module/source metadata.

Acceptance:

- Sample failures across all seven categories produce both pretty and `--json`
  outputs that round-trip through `serde`.
- Stack frames map to the original `.ts` source positions.

## P3: Performance And Scale

- [ ] Add startup benchmark for empty runtime, one-file run, package-resolved
  run.
- [ ] Add resolver benchmark for large package trees.
- [ ] Add compiler benchmark for TS-heavy files.
- [ ] Add object/property microbenchmarks.
- [ ] Add module-graph cache and invalidation strategy.
- [ ] Add package metadata cache with corruption detection.
- [ ] Add lockfile-graph memory benchmark.

Acceptance:

- Every P3 patch includes before/after numbers (criterion or scripted).
- Slow paths are visible in `--trace` / `--cpu-prof` output.

## Immediate Iteration Queue

1. Create `otter-pm-manifest`, `otter-pm-lockfile`, and `otter-pm` skeletons
   and wire them into the workspace.
2. Define manifest and lockfile data models with roundtrip tests.
3. Land the runtime hook trait set; extend `Runtime`/`RuntimeBuilder` to own
   the session state listed in P0.1.
4. Route CLI `run` and the file-shorthand path through the runtime hooks.
5. Add `PackageGraph` lookup to the existing `ModuleLoader`.
6. Add `otter install` command for registry metadata + lockfile-only resolve
   (no extract yet).
7. Add tarball cache/extract and bin linking; wire them into `otter run`.
8. Add JSON, conditional-exports, and `tsconfig` `paths` fixtures.
9. Wire `otter check` and `otter test` through the same resolver/package graph
   as `otter run`.
10. Drive the remaining function/arguments/object descriptor work onto the
   centralized builder/bootstrap path.
11. Delete replaced bridge/shim code immediately after each slice lands; list
    the deleted symbols in the slice commit.

## Quality Gates

Before each merged slice, run the applicable stable subset:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p otter-vm
cargo test -p otter-runtime
```

After the P0.2 crates exist, package-manager slices also run:

```bash
cargo test -p otter-pm-manifest
cargo test -p otter-pm-lockfile
cargo test -p otter-pm
```

When the slice touches ECMAScript semantics, run the matching Test262
sub-tree (replace the example pattern):

```bash
just test262-filter "built-ins/Object"
```

When the slice touches CLI/package manager:

```bash
cargo run -p otter-cli -- install
cargo run -p otter-cli -- add <fixture-package>
cargo run -p otter-cli -- run <fixture-entry>
cargo run -p otter-cli -- check <fixture-entry>
cargo run -p otter-cli -- test
```

`otter-cli`'s `[[bin]]` name is `otter`, so the binary on `$PATH` after
`cargo install` is `otter`, but the cargo crate id stays `otter-cli`.

## Removal List

- [ ] Remove duplicated module-loader paths once the unified hook set lands.
- [ ] Remove the `module_api` shim and any remaining VM-shaped public types
  re-exported from product crates (carry-over from
  `RUNTIME_PUBLIC_API_PLAN.md`).
- [ ] Remove any ad-hoc global mutation found outside `bootstrap.rs` /
  builders.
- [ ] Remove runtime bridges once the runtime hook set owns them.
- [ ] Remove stale docs that describe compatibility shims as current behavior.
- [x] Delete parked `crates-legacy/otter-pm*`; active PM crates under
  `crates/*` are the only package-manager implementation.

## Open Questions

- None for P0.2 naming and UX: active crate names are `otter-pm-manifest`,
  `otter-pm-lockfile`, `otter-pm`; lockfile filename is `otter.lock`;
  package-manager commands do not require runtime capability flags;
  `pnpm-workspace.yaml` is P0.
