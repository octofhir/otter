# Task 95 — Contributor book and extension/plugin guides

## Status

- [ ] `docs/book/` skeleton lands with `book.toml` and `src/SUMMARY.md`
- [ ] local build command documented (`mdbook build docs/book` or
      project-approved equivalent)
- [ ] contributor guide covers repository map, build/test loop, and
      task workflow
- [ ] engine internals guide covers VM, bytecode, runtime boundary,
      GC, async, and modules
- [ ] extension/plugin guide covers hosted modules, native bindings,
      permissions, and future plugin ABI direction
- [ ] macro guide covers `#[js_class]`, `#[js_namespace]`, `#[dive]`,
      `raft!`, `burrow!`, `lodge!`, and future GC trace macros
- [ ] book examples compile or have tracked expected-output tests
- [ ] docs CI checks links, stale snippets, and mdBook build
- [ ] gates green

## Goal

Start treating Otter's contributor documentation as a product surface,
not an afterthought. The engine should be easy to extend without
copy-pasting internal code or reverse-engineering task files.

The initial format is `mdBook` because it is simple, static,
Markdown-first, and familiar in Rust projects. If we later choose a
different generator, preserve the same source layout and build gates.

## Source

- [`70-gc-master-tracker.md`](./70-gc-master-tracker.md) documentation
  rule.
- [`94-gc-contributor-api-surface.md`](./94-gc-contributor-api-surface.md)
  safe GC / VM contributor API.
- [`93-gc-branded-session-api.md`](./93-gc-branded-session-api.md)
  branded GC/session model.
- Existing macro rules in repository `AGENTS.md`.
- `docs/new-engine/repository-map.md`, ADRs, and task files.

## Scope

### 95.1 — Book skeleton

Create:

```text
docs/book/book.toml
docs/book/src/SUMMARY.md
docs/book/src/introduction.md
docs/book/src/contributing/overview.md
docs/book/src/engine/architecture.md
docs/book/src/engine/gc-api.md
docs/book/src/extensions/overview.md
docs/book/src/extensions/hosted-modules.md
docs/book/src/extensions/plugin-system.md
docs/book/src/macros/overview.md
```

The skeleton should link to current ADRs/tasks instead of duplicating
unstable details. As APIs stabilize, copy the stable user-facing
workflow into the book and leave task files as implementation history.

### 95.2 — Contributor guide

Cover:

- workspace layout and active vs parked crates;
- how to choose the right crate for a change;
- build/test commands and fast iteration loops;
- conformance workflow and when to update `ES_CONFORMANCE.md`;
- how to close task files;
- unsafe policy and where unsafe is permitted;
- breaking-change policy for `crates-next/*`;
- how to write tests, compile-fail tests, and docs examples.

### 95.3 — Engine internals guide

Cover:

- parser/compiler/bytecode/VM pipeline;
- runtime boundary and `RuntimeCx` / `NativeCtx`;
- GC model, handle tiers, branded sessions, weak/finalization policy,
  and backing-store accounting;
- async/event-loop model and host operation scheduling;
- module loading and permission model;
- profiling/debugging workflows.

This is the "how to modify the engine safely" guide.

### 95.4 — Extension and future plugin guide

Cover the public extension model in layers:

1. hosted modules inside the workspace;
2. native bindings/macros compiled with the engine;
3. future out-of-tree plugin package model;
4. future ABI/FFI boundary if we support dynamically-loaded plugins.

Document the non-negotiables:

- permissions are deny-by-default;
- no GC handle crosses isolate/worker boundaries;
- persistent state uses `Root`, not raw `Gc`;
- weak handles upgrade only through a branded context;
- external memory is accounted through RAII tokens;
- plugin APIs must not expose raw collector internals by default.

### 95.5 — Macro guide

Document macro intent, examples, generated shape, and safety limits:

- `#[js_class]` for constructor-backed JS classes;
- `#[js_namespace]` for namespace objects;
- `#[dive]` / `#[dive(deep)]` for sync/async native bindings;
- `raft!` for grouped bindings;
- `burrow!` for host-owned object surfaces;
- `lodge!` for hosted modules;
- future `#[derive(GcTrace)]` / field attributes from task 94.

The macro guide must state when manual code is preferred: capability
enforcement, delicate bootstrap/install order, or control flow that a
macro would hide.

### 95.6 — Docs build and CI

Add one local command and one CI gate:

```bash
mdbook build docs/book
```

If `mdbook` is not installed in CI, either install it in the docs job or
use a pinned Rust tool wrapper. Do not make normal `cargo test` depend
on network access.

CI should eventually check:

- book builds;
- internal links resolve;
- code examples compile or are explicitly marked ignored with a reason;
- generated API examples do not drift from actual macro signatures.

## Out of scope

- Publishing docs to a public website in this task. Build the book
  locally/CI first; hosting can be a later deployment task.
- Freezing the plugin ABI. This task documents direction and safe
  extension layers, not a stable ABI promise.
- Duplicating every task file. The book should summarize stable
  contributor workflows and link to task/ADR details.

## Validation gates

- [ ] `mdbook build docs/book` succeeds locally.
- [ ] Book has pages for contributor workflow, GC API, hosted modules,
  future plugin system, and macros.
- [ ] Task 94 validation examples are either in the book or linked from
  it.
- [ ] New docs CI job is green or tracked as a separate infrastructure
  task with an explicit owner.
- [ ] No broken relative links in `docs/book/src/SUMMARY.md`.

## Closing

Tick task 95 in [70-gc-master-tracker.md](./70-gc-master-tracker.md).
Update `AGENTS.md` if the contributor workflow or macro guidance changes.
