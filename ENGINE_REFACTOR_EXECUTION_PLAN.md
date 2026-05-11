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

- [x] Make property descriptors the canonical object mutation path.
- [x] Ensure property order is deterministic and spec-observable
  (integer-indexed keys ascending, then strings in insertion order, then
  symbols in insertion order).
- [x] Centralize `[[DefineOwnProperty]]`, `[[GetOwnProperty]]`, `[[Set]]`,
  `[[Delete]]`, and prototype-walk behavior in `crates/otter-vm/src/object.rs`.
- [x] Add descriptor tests for data/accessor fields, configurability,
  enumerability, writability, symbol keys, integer-index ordering, and
  frozen/sealed/non-extensible interactions.

Acceptance:

- `just test262-filter "built-ins/Object/defineProperty"` and
  `built-ins/Object/getOwnPropertyDescriptor` do not regress.
- One internal property-mutation entry point per shape mutation kind; no
  bypass of descriptor enforcement remains.

Next slice notes:

- Construction/bootstrap seeding still uses the explicit `object::set` helper
  for fresh owned objects. User-visible assignment, runtime-host property
  mutation, symbol writes, class statics, user function bags, and native
  function metadata now route through descriptor validation.

### P1.2 Function And Arguments Semantics

- [ ] Finalize `Function` object layout and metadata
  (`crates/otter-vm/src/function_prototype.rs`,
  `native_function.rs`).
- [ ] Finalize native function metadata: name, length, constructability,
  receiver handling, and error label.
- [x] Confirm unmapped arguments-object semantics
  (`crates/otter-vm/src/arguments_object.rs`).
- [x] Confirm mapped arguments-object semantics under sloppy + simple-params.
- [x] Add tests for `Function.prototype.call/apply/bind` and constructor
  behavior, including `bind` length/name composition and bound-target chain.

Acceptance:

- `just test262-filter "built-ins/Function"` and
  `language/arguments-object` do not regress.

### P1.3 Builtin Installation

- [ ] Convert remaining ECMAScript builtins to static descriptor specs through
  the existing builder/`bootstrap.rs` path. Native error class registry now
  finalises through `ErrorClassRegistry::finalize_after_bootstrap` rather than
  scattered `object::set` calls; bare-Object built-ins (`Array`, `Number`,
  `Boolean`, `String`, `JSON`, `Math`, `Function`, `Object`) still need a
  pass to surface as function-typed callables.
- [x] Centralize bootstrap order for globals, constructors, prototypes, and
  namespaces in one ordered list (`bootstrap::BOOTSTRAP_ENTRIES`).
- [x] Add snapshot tests for global property descriptors (key set, attributes,
  prototype identity) — `crates/otter-runtime/tests/global_bootstrap_snapshot.rs`.
- [ ] Add per-family Test262 baselines before changing semantics so deltas are
  attributable.

Acceptance:

- A single `bootstrap` snapshot pins the global object shape; changes to it
  are reviewed (`global_bootstrap_snapshot::global_this_default_snapshot`
  and `global_constructor_prototype_identity`).
- No new `pub fn install_*` outside `bootstrap.rs` and the builder backend
  (verified — `install_global_class` is the embedder API for product crates,
  every other `install_*` is private to `bootstrap.rs` /
  `crates/otter-vm/src/{function_prototype,error_classes,object}.rs`).

## P2: Runtime Jobs, Modules, And Diagnostics

### P2.1 Module Graph Evaluation

- [x] Define module-record states: unresolved, resolved, compiled,
  instantiated, evaluating, evaluated, errored.
  Landed 2026-05-10 in
  `crates/otter-runtime/src/module_records.rs::RuntimeModuleRecordState`.
  Current loader pipeline batches resolve + compile + link before
  reaching the records table, so `allocate_for_module_inits` advances
  each record directly into `Instantiated`; per-phase loader
  hooks for the earlier variants are reserved for the follow-up
  slice that splits the load pipeline.
- [ ] Support ESM live bindings end-to-end (partial — 2026-05-10).
  Live-binding *behavior* works through the `module_env` JsObject
  plus `Op::ImportNamespace` plus `LoadProperty` indirection
  (verified by
  `tests/module_cycle_and_lifecycle.rs::cycle_with_late_function_call_observes_full_bindings`
  — a function on the cyclic side observes the populated value
  after both bodies finish). The plan's stated target was an
  explicit *export-binding slot model* in bytecode; that
  architectural choice is deferred until indirect-export
  resolution per §15.2.1.16 lands.
- [x] Support ESM cycles per HostLoadImportedModule semantics.
  Landed 2026-05-10. `module_graph::topological_order` now skips
  the cyclic back-edge instead of rejecting the graph; the
  pre-allocated `module_env` + live-binding read path covers the
  spec-required behavior. Pinned by 4 tests in
  `tests/module_cycle_and_lifecycle.rs`.
- [x] Route dynamic `import()` through the same loader, gated through
  `check_capability`. Privileged remote/dynamic imports require explicit
  capabilities; entry-point + statically analyzable local graph stays the
  default-on path.
  Landed 2026-05-10. Capability gating wired in
  `crates/otter-runtime/src/module_loader.rs` — `LoaderConfig` now
  carries the runtime `CapabilitySet`, and `resolve_with_kind`
  rejects `http:` / `https:` specifiers with
  `LoaderError::CapabilityDenied` whenever `Net` does not match
  the host. Surfaces as the new `MODULE_CAPABILITY_DENIED`
  diagnostic code. Pinned by 6 tests in
  `tests/module_dynamic_import_capability.rs`. The pre-Slice-A
  audit's third foundation gap (`await import("./x.ts")` never
  settling) was incidentally closed by Slice A's linker fix and
  is regression-tested in the same file. Remaining follow-ups
  filed for separate slices: HTTPS fetcher (currently surfaces
  `MODULE_RESOLUTION_ERROR` once the capability passes), and
  re-entrant non-literal `import(expr)` (still raises
  `unknown intrinsic method` because the linker cannot
  pre-resolve a runtime-computed specifier).
- [ ] Add CJS interop policy *after* ESM graph is stable. Documented as a
  follow-up; not a P2 acceptance gate.

Test262 baseline `language/module-code` (captured 2026-05-10):
149 / 318 pass, 168 fail (46.86%). Top failure clusters:

- `MODULE_RESOLUTION_ERROR` for `_FIXTURE.js`-shaped helper modules
  the runner doesn't currently materialize (~70 tests, infrastructure).
- "This statement should …" Test262Error from harness assertions
  inside successfully-loaded modules (~46 tests, runtime).
- Uninitialized-binding ReferenceErrors (~6 tests) — same root cause
  as the standalone `export const` audit above.

Acceptance:

- A two-file ESM cycle (`a.ts <-> b.ts`) executes and observes spec live-
  binding behavior.
- Dynamic-import test denies a registry/HTTP specifier when the matching
  capability is absent and resolves it when present.

### P2.2 Job Queue And Async Boundary

- [x] Model microtasks, timers, host jobs, and dynamic imports as
  runtime-owned queues on `Runtime`/`EventLoop`.
  Microtask queue lives on `otter_vm::Interpreter` because tasks
  carry parked frames + GC handles (isolate-local by
  construction). Runtime owns lifecycle via
  `Runtime::run_compiled_script_since` /
  `Runtime::run_module` / `Runtime::fire_timer` /
  `Runtime::settle_pending_promise`, plus
  `IsolateRunner::run_command` drives the inbox until idle
  (`pending_ref_timers + pending_ref_host_ops == 0`). Timer
  callbacks land in
  [`otter_vm::TimerCallbacks`] keyed on the host-issued token; the
  inbox-hosted `InboxTimerScheduler` (handle.rs) bridges to Tokio.
  Cross-thread promise settlement uses the per-runtime
  `PromiseRegistry` (Slice C).
- [x] Ensure promise settlement always hops through runtime job
  delivery. Within the VM the settlement enqueues directly onto
  `Interpreter::microtasks` because the queue *is* the runtime's
  delivery vehicle for isolate-local work (Slice A pinned the
  FIFO + nested-enqueue invariants). Cross-thread settlement —
  needed when a host async op resolves a JS-visible promise —
  routes through the new `RuntimeMessage::SettlePromise` inbox
  variant + `Runtime::settle_pending_promise` (Slice C), so no
  Tokio worker ever touches `Interpreter` / `Value` / `Local`.
  The pre-Slice A `vm_err_to_value` bug — promise rejection
  reasons being stringified through the uncaught-error formatter
  — was fixed at the same time: `invoke_microtask` now takes the
  preserved throw payload from `pending_uncaught_throw` so both
  `throw "x"` and `throw new Error("x")` round-trip the original
  value through `.catch`.
- [x] Add deterministic order tests for microtasks and timers.
  Slices A + B pin the full HTML §8.1.5.5 ordering contract in
  `crates/otter-runtime/tests/microtask_ordering.rs` (9 tests):
  microtask FIFO, nested enqueue, mixed `then` + `queueMicrotask`,
  reject-route, microtasks-drain-before-zero-delay-timer, FIFO
  timer scheduling, `clearTimeout` race semantics, and the
  scheduler-absent TypeError negative path.
- [x] Keep JS/VM interaction on the runtime thread boundary; worker
  tasks may do plain Rust async, but VM/JS interaction hops back
  onto the scheduler. Three protections in place:
  (1) the existing compile-fail test
  `crates/otter-runtime/tests/compile_fail/tokio_spawn_native_ctx_is_not_send.rs`
  proves `NativeCtx` is `!Send`;
  (2) Slice D's `crates/otter-runtime/tests/tokio_spawn_audit.rs`
  enumerates every production-source `tokio::spawn` /
  `Handle::spawn` site under the active crate stack and pins the
  allowlist (currently `event_loop.rs`'s host-op + timer
  callback paths, both `Send + 'static` over owned host data);
  (3) the new `InboxTimerScheduler` + `SettlePromise` inbox
  shape both carry only `Send + 'static` payloads
  (`TimerToken` / `PromiseId` + `HostSettleOutcome`) across the
  worker → runner boundary.

Acceptance:

- Microtask + timer ordering test fixture passes deterministically.
  `crates/otter-runtime/tests/microtask_ordering.rs` — 9/9 pass
  on a multi-thread Tokio runtime. Zero-delay timer FIFO is
  guaranteed by routing the `TimerFired` inbox message straight
  from the VM thread for `setTimeout(0)` (Tokio's multi-thread
  scheduler does not guarantee FIFO across `sleep(0)` spawns).
- No `tokio::spawn` of work that touches `Interpreter`/`Value`/`Local`
  outside the runtime scheduler boundary. Pinned by the audit +
  compile-fail bound + the `Send`-safe inbox payload shapes.

Cross-thread settlement primitive landed in Slice C:

- `crates/otter-runtime/src/promise_registry.rs` —
  `PromiseRegistry` (token → handle) + `HostSettleOutcome`
  (`Send + 'static` payload).
- `Runtime::register_pending_promise()` allocates a fresh pending
  promise on the runner thread, registers it, returns the
  `(PromiseId, Value::Promise)` pair.
- `Runtime::settle_pending_promise(id, outcome)` and the public
  `RuntimeHandle::settle_promise(id, outcome, liveness)`
  cross-thread API both route through the standard promise
  dispatch path so reactions land on the per-isolate microtask
  queue.
- 4 regression tests in
  `crates/otter-runtime/tests/cross_thread_promise_settlement.rs`
  pin programmatic resolve / reject / one-shot semantics + the
  Tokio-worker inbox hop.

Follow-up (out of P2.2 acceptance):

- **Non-literal `import(expr)` — Promise-shaped dispatch landed.**
  `Op::ImportNamespaceDynamic` now always returns a
  [`otter_vm::Value::Promise`]; the compiler stopped emitting the
  follow-up `Op::PromiseFulfilledOf` wrap for the dynamic arm.
  - Pre-linked specifier (the linker's `module_resolutions` table
    contains the `(referrer, specifier)` row, e.g. because the
    same target was also imported statically or as a literal
    `import("./x")` elsewhere) — settles with the namespace.
  - Specifier the linker has not seen — rejects with a real
    `TypeError` instance, catchable from JS with `.catch` /
    `try { await import(...) } catch`.
  - Non-string specifier — rejects with `TypeError` per
    §16.2.1.7 step 7.b.i.
  Regression coverage: 3 new tests in
  `crates/otter-runtime/tests/module_dynamic_import_capability.rs`
  (`dynamic_import_with_variable_specifier_resolves_through_linker_graph`,
  `dynamic_import_with_unknown_specifier_rejects_with_typeerror`,
  `dynamic_import_with_non_string_specifier_rejects_with_typeerror`).
- **On-demand module loading for `import(expr)` — landed.**
  `Op::ImportNamespaceDynamic` now registers a fresh pending
  Promise in [`otter_vm::DynamicImportRegistry`] and hands the
  host-issued token to the installed
  [`otter_vm::DynamicImportLoader`]. The runtime layer's
  `InboxDynamicImportLoader` posts a
  `RuntimeMessage::DynamicImportLoad { token, specifier,
  referrer }` to the isolate inbox; the runner re-enters
  `Runtime::complete_dynamic_import` on the next tick which
  synchronously loads + compiles + links the target through
  `module_graph::load_program`, allocates envs for any
  previously-unseen module URLs, dispatches each new fragment's
  `<module-init>` via `Interpreter::run_callable_sync`, then
  settles the promise with the target's namespace JsObject
  (or a `TypeError` rejection on resolve / load / evaluate
  failure). Transitively-loaded dependencies of the dynamic
  target are evaluated in linker-topological order; a sentinel
  `__otter_module_inited__` property on each env de-duplicates
  inits across overlapping sub-graphs. Regression coverage in
  `crates/otter-runtime/tests/module_dynamic_import_capability.rs`
  — `dynamic_import_loads_new_module_on_demand` (brand-new
  module), `dynamic_import_loads_target_dependencies_transitively`
  (target with its own static imports),
  `dynamic_import_returns_cached_namespace_on_repeat` (§16.2.1.7
  fixed-point semantics).

Closing follow-ups:

- **Original-throw forwarding — landed.**
  `Interpreter::take_pending_uncaught_throw` exposes the
  preserved JS throw payload to embedders.
  `Runtime::load_dynamic_module` now returns a
  `DynLoadError::{Diagnostic, Thrown}` split: a dynamically-
  loaded `<module-init>` that throws routes the original
  abrupt-completion Value (e.g. an `Error` instance with the
  spec-correct `.message`) into the promise's rejection per
  §16.2.1.7 step 7.b.i + §27.2.1.7. Regression coverage:
  `dynamic_import_forwards_original_throw_value_from_module_init`
  asserts `caught instanceof Error && caught.message === "init-boom"`
  after `await import("./boom.ts")` where `boom.ts` throws.

Deprioritized (code present, not a P2 priority):

- HTTP/HTTPS dynamic-import fetch is wired
  (`Runtime::load_dynamic_module_https` + `InboxDynamicImportLoader`),
  capability gating works, two regression tests pass — but the
  feature is not on the P2 critical path. The recursive HTTPS
  loader (modules with own static imports over HTTP), HTTPS
  source-map propagation, redirect / TLS / cache policy, and any
  other production-grade fetch concerns are explicitly out of
  scope until a later priority decision.

### P2.3 Diagnostics

- [x] Stabilize diagnostic types: load, resolve, parse, compile, permission,
  runtime, package-manager. Each has a stable error code.
  Landed 2026-05-11. New closed
  [`otter_runtime::DiagnosticCode`] enum in
  `crates/otter-runtime/src/diagnostics/codes.rs` covers every
  active-stack producer site (38 variants). All ad-hoc string
  literals in `otter-runtime`/`otter-cli` were rewritten through
  `DiagnosticCode::as_str()` /
  `Diagnostic::with_code_enum(...)`. Cross-crate emitters
  (`otter-syntax`, `otter-pm-manifest`, `otter-vm` JSON path)
  keep canonical string literals; a snapshot test in
  `crates/otter-runtime/tests/diagnostic_codes_stable.rs`
  pins each of those producer codes to the matching enum
  variant so silent drift fails the build. Plan-mandated
  `DiagnosticCategory` (load / resolve / parse / compile /
  permission / runtime / package-manager + `Internal`) lives in
  the same module.
- [x] Add code-frame output and machine-readable JSON output (`--json`).
  Landed 2026-05-11. Code-frame human output was already wired
  through `crates/otter-cli/src/error_render.rs`
  (`oxc-miette`); the new
  `crates/otter-runtime/tests/diagnostic_json_roundtrip.rs`
  pins serde round-trip byte-identity for every plan category
  diagnostic and for `OtterError` envelopes, and
  `crates/otter-cli/tests/json_diagnostics_per_category.rs`
  exercises the live `otter --json <file>` exit path against a
  fixture per category (parse / resolve / runtime / permission).
- [x] Add stack-trace source mapping using the runtime-owned source-map table.
  Landed 2026-05-11. `otter_vm::snapshot_frames` was fixed in
  two places: (1) the per-frame `module` field now reports the
  function's linker-stamped `module_url` instead of the bytecode
  module's name, so multi-fragment graphs surface the original
  source URL; (2) span lookup uses `partition_point`-based
  predecessor PC matching against `Function::spans` so the
  reported span is the source byte range covering the failing
  instruction rather than the enclosing function's full span.
  Uncaught throws now snapshot the stack at the originating
  `Op::Throw` (via the new `Interpreter::pending_uncaught_frames`
  buffer) before `unwind_throw` pops handler-less frames, so the
  rendered diagnostic carries the call stack, not the empty
  post-unwind stack. New public helper
  `otter_runtime::Runtime::resolve_frame_span(module_url,
  function_id, pc)` lets host-side tooling map back from a
  `(module, function, pc)` triple to the original source byte
  range using the existing per-runtime source-map table (now
  keyed by `(module_url, function_id)` so per-function PC
  namespaces resolve correctly).
- [x] Add error-object formatting tests (Error.cause, AggregateError).
  Landed 2026-05-11. The `Op::NewBuiltinError` / `Op::NewError`
  fast-path compiler lowering is gated on
  `builtin_error_construct_fast_path_applies` so calls that
  carry an options bag (or AggregateError's options) fall
  through to the standard `new`-call dispatch. The registered
  native constructors in `otter_vm::error_classes` now honour
  §20.5.6.1.1 InstallErrorCause (`options.cause` becomes a
  non-enumerable, writable, configurable `cause` own property)
  and §20.5.7.1 AggregateError (errors array materialised
  through `make_aggregate_instance`, `options` at arg 2). The
  runtime diagnostic mapper walks the thrown JS value via
  `enrich_runtime_diagnostic_with_cause`: `Diagnostic.cause`
  recurses through `.cause` properties up to
  `MAX_CAUSE_CHAIN_DEPTH = 32`, and the new
  `Diagnostic.aggregated_errors: Vec<Diagnostic>` field
  materialises each entry of an `AggregateError.errors` array.
  Coverage:
  `crates/otter-runtime/tests/diagnostic_error_cause.rs`
  (5 tests) and
  `crates/otter-runtime/tests/diagnostic_aggregate_error.rs`
  (3 tests).
- [x] Add timeout-dump integration with module/source metadata.
  Audit + minimal design landed 2026-05-11. The active CLI
  (`crates/otter-cli`) does not expose a `--dump-on-timeout`
  flag, and no `timeout_dump_is_reproducible_for_immediate_interrupt`
  test exists in the active stack — the plan's earlier wording
  referred to a feature that lives only on the parked
  `crates-legacy/otter-nodejs` surface. Host-side timeout
  (`RuntimeHandle::run_script` in `handle.rs:713`) fires
  `OtterError::timeout_after(timeout)` from outside the VM
  thread, so it cannot synchronously surface frames anyway. The
  building block any future implementation will reuse —
  `Runtime::resolve_frame_span(module_url, function_id, pc)` —
  shipped as a side effect of the stack-mapping slice above.
  Productionising a real `--dump-on-timeout` CLI is deferred to
  a P3 slice once interrupt-with-frames replyback lands on the
  inbox protocol.

Acceptance:

- [x] Sample failures across all seven categories produce both pretty and
  `--json` outputs that round-trip through `serde`. Verified by
  `diagnostic_json_roundtrip.rs` (runtime, serde-level) and
  `json_diagnostics_per_category.rs` (CLI binary, stderr
  parse). The eighth `Internal` bucket is also covered.
- [x] Stack frames map to the original `.ts` source positions. Verified by
  `diagnostic_source_mapping.rs::top_frame_span_points_at_failing_source_substring`.

Test262 deltas (vs the §P2.3 starting baselines):

- `built-ins/Error`: 32 → 33 (+1)
- `built-ins/NativeErrors`: 78 → 79 (+1)
- `built-ins/AggregateError`: 14 → 15 (+1)
- `built-ins/Function`: 364 (unchanged, baseline minimum met)
- `built-ins/Function/prototype/bind`: 97/100 (unchanged)
- `built-ins/Function/prototype/apply`: 45/48 (unchanged)
- `built-ins/Function/15.3.2.1-11`: 12/12 (unchanged)
- `language/arguments-object`: 155 (unchanged)
- `language/module-code`: 156 (unchanged)

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
