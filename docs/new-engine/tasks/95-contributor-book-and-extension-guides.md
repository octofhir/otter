# Task 95 — Contributor book and extension/plugin guides

## Status

- [x] `docs/book/` skeleton lands with `book.toml` and `src/SUMMARY.md`
- [x] local build command documented (`mdbook build docs/book` or
      project-approved equivalent)
- [x] contributor guide covers repository map, build/test loop, and
      task workflow
- [x] engine internals guide covers VM, bytecode, runtime boundary,
      GC, async, and modules
- [x] event-loop guide covers task-85 drive modes, runtime inbox,
      microtask checkpointing, async host-op boundary, and ref/unref
      liveness
- [x] extension/plugin guide covers hosted modules, native bindings,
      permissions, and future plugin ABI direction
- [x] JS surface guide covers task-96 specs/builders/bootstrap registry
- [x] macro guide covers task-97 zero-cost macros as syntax sugar over
      static specs, including generated-shape examples
- [x] startup/performance guide covers task-98 cold-start benchmarks and
      bootstrap budgets
- [x] book examples compile or have tracked expected-output tests
- [x] docs checks cover stale snippets, mdBook build locally, and
      GitHub Pages deployment workflow
- [x] gates green

## Goal

Treat Otter's contributor documentation as a product surface, not an
afterthought. The engine should be easy to extend without copy-pasting
internal code or reverse-engineering task files.

`docs/book/` is the canonical contributor-facing home for "what this is"
and "how to work on it". Task files remain implementation plans and
closeout history. When a contributor API stabilizes, move the workflow and
examples into the book and leave the task file as a pointer.

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
- [`96-production-js-surface-builders.md`](./96-production-js-surface-builders.md)
  production JS surface specs, builders, and bootstrap registry.
- [`97-zero-cost-js-surface-macros.md`](./97-zero-cost-js-surface-macros.md)
  zero-cost macro generation over static specs.
- [`98-startup-bootstrap-performance.md`](./98-startup-bootstrap-performance.md)
  startup benchmark and bootstrap budget ratchets.
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
docs/book/src/engine/event-loop.md
docs/book/src/engine/gc-api.md
docs/book/src/extensions/overview.md
docs/book/src/extensions/hosted-modules.md
docs/book/src/extensions/native-bindings.md
docs/book/src/extensions/js-surface-builders.md
docs/book/src/extensions/plugin-system.md
docs/book/src/performance/startup.md
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
- why `docs/book/` is the stable contributor API documentation home;
- unsafe policy and where unsafe is permitted;
- breaking-change policy for `crates-next/*`;
- production-readiness policy: breaking changes are allowed when they
  remove unsoundness risk, runtime-only checks, thread-local coupling,
  startup regressions, or compatibility shims;
- how to write tests, compile-fail tests, and docs examples.

### 95.3 — Engine internals guide

Cover:

- parser/compiler/bytecode/VM pipeline;
- runtime boundary and `RuntimeCx` / `NativeCtx`;
- task-85 event-loop model, runtime inbox vs microtask queue, drive modes,
  ref/unref liveness, cancellation, backpressure, and diagnostics;
- GC model, handle tiers, branded sessions, weak/finalization policy,
  and backing-store accounting;
- async/event-loop model and host operation scheduling;
- module loading and permission model;
- centralized builtin/bootstrap registry and install order;
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
- JavaScript-visible objects, namespaces, classes, functions, accessors,
  and hosted module surfaces are installed through task-96 specs/builders
  and the centralized bootstrap registry.

### 95.5 — JS surface builder guide

Document the task-96 API as the primary contributor workflow for adding
JavaScript-visible surfaces:

- `Attr` / property attribute defaults;
- `PropertySpec`, `MethodSpec`, `AccessorSpec`, `ConstructorSpec`,
  `ClassSpec`, and `NamespaceSpec`;
- `ObjectBuilder`, `FunctionBuilder`, `ConstructorBuilder`,
  `ClassBuilder`, and `NamespaceBuilder`;
- `NativeCall::Static` / static function-pointer fast path for builtins;
- when a dynamic boxed closure is acceptable;
- centralized bootstrap registration and deterministic install order;
- feature/capability gating at install time;
- performance rules: no per-call allocation, no runtime metadata parsing,
  no hot-path dynamic registry.

The examples must be buildable or explicitly marked ignored with a reason.

### 95.6 — Macro guide

Document macro intent, examples, generated shape, and safety limits:

- `#[js_class]` for constructor-backed JS classes;
- `#[js_namespace]` for namespace objects;
- `raft!` or equivalent grouped-spec macro;
- future `#[dive]` / async native binding sugar only after task 85's
  event-loop boundary and task 96's native fast path are stable;
- future host-owned object / hosted-module / GC trace macros only after
  their backend APIs are stable.

The macro guide must state that macros generate task-96 static specs and
normal Rust functions. They must not generate runtime registries, per-call
allocations, metadata parsing, hidden permission checks, hidden async
scheduling, or hidden global mutation.

The macro guide must state when manual code is preferred: capability
enforcement, delicate bootstrap/install order, or control flow that a
macro would hide.

### 95.7 — Startup and performance guide

Document:

- how to run cold startup and first-run benchmarks from task 98;
- how to read bootstrap telemetry;
- current startup budgets and regression policy;
- what changes require before/after benchmark tables;
- why high-level contributor APIs must compile down to the same runtime
  shape as handwritten static specs.

### 95.8 — Docs build and CI

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
- Treating macros as the primary API before task 96's builder/spec backend
  exists. Macro docs describe generated shape and usage after the backend
  is stable.

## Validation gates

- [x] `mdbook build docs/book` succeeds locally.
- [x] Book has pages for contributor workflow, GC API, hosted modules,
  event-loop/async boundary, JS surface builders/bootstrap, startup
  performance, future plugin system, and macros.
- [x] Task 94 validation examples are either in the book or linked from
  it.
- [x] Task 96/97/98 examples and benchmark commands are either in the book
  or linked from it once those tasks land.
- [x] GitHub Pages workflow added in
  `.github/workflows/docs-pages.yml`; local docs gate is green.
- [x] No broken relative links in `docs/book/src/SUMMARY.md`.

## Closing

Tick task 95 in [70-gc-master-tracker.md](./70-gc-master-tracker.md).
Update `AGENTS.md` if the contributor workflow or macro guidance changes.

## Progress Notes

- 2026-05-06: expanded the mdBook from skeleton pages into the
  contributor workflow for the active `crates-next/*` stack:
  repository/crate selection, task closeout rules, ES conformance
  workflow, unsafe boundary, engine pipeline, runtime/native context
  boundary, GC handle tiers, event-loop queues/drive modes/ref-unref
  liveness, hosted modules, native bindings, future plugin layering,
  Task 96 builder/spec direction, Task 97 macro generated-shape contract,
  and Task 98 startup benchmark workflow.
- 2026-05-06: added
  `crates-next/otter-gc/tests/book_gc_api_examples.rs` as buildable
  examples backing the book's GC API page. The examples cover
  `EscapableHandleScope`, `ExternalMemory`, and branded `Root` / `Weak`
  session usage. Existing rustdoc examples for `EscapableHandleScope` and
  `ExternalMemory` remain green.
- 2026-05-06: Task 96/97 APIs are documented as future/deferred and all
  such examples are explicitly `ignore` snippets. The book does not
  promise macro/plugin APIs that have not landed.
- 2026-05-06: docs checks:
  - `mdbook build docs/book` green.
  - static stale-wording scan for closed GC-surface blocker wording and
    unsafe/raw API guidance green.
  - `docs/book/src/SUMMARY.md` links the contributor, engine, extension,
    performance, and macro chapters.
- 2026-05-06: added `.github/workflows/docs-pages.yml` to build the
  mdBook on docs PRs and deploy `docs/book/book` to GitHub Pages from
  `main` using the official Pages artifact/deploy flow. The workflow pins
  `mdbook` to `0.4.52`.
- 2026-05-06: validation gates green:
  - `cargo fmt --all`
  - `cargo test -p otter-gc --test book_gc_api_examples`
  - `cargo test -p otter-gc -p otter-vm -p otter-runtime`
  - `cargo test --workspace`
  - `mdbook build docs/book`
