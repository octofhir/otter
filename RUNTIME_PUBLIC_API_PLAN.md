# Runtime Public API Plan

This plan tracks the Rust-first active runtime migration. The target shape is:

```text
otter-gc -> otter-vm -> otter-runtime -> product crates
```

Product crates must not depend on `otter-vm` for hosted modules, Web API
bootstrap, permissions, or native binding ergonomics. VM details may exist behind
runtime-owned adapter code while the migration is in progress, but they should
not become the public embedding contract.

## Non-goals

- Do not revive or depend on `crates-legacy/*`.
- Do not introduce a JIT or optimizing compiler path as part of this work.
- Do not store `RuntimeCx`, `NativeCtx`, `Value`, `Gc`, or `Local` in async
  futures.
- Do not add hot-path dynamic registries or runtime metadata parsing for static
  builtins/modules.
- Do not disable spec-important surfaces such as Intl, Temporal, or RegExp.

## P0: VM boundary and public API

- [x] Replace public hosted module installation signatures that mention
  `Interpreter`, `JsObject`, `Value`, or other VM internals.
- [x] Add a runtime-owned hosted module builder API for static `otter:*`
  modules.
- [x] Move `otter-modules` off direct `otter-vm` dependencies.
- [x] Replace `RuntimeBuilder::global_class(&'static otter_vm::ClassSpec)` with a
  runtime-owned global surface API.
- [ ] Add runtime-owned builder primitives for namespaces, classes, methods,
  accessors, constants, and prototypes.
- [x] Add a host-owned object primitive so Web APIs do not use per-instance
  `Arc<Mutex<_>>` dynamic closures to hold state.
- [x] Add native method call context with receiver and constructor/new-target
  information.
- [ ] Make capability defaults and source/module loading deny-by-default at the
  Rust boundary.
- [ ] Add compile-fail tests for non-`Send`/non-`Sync` VM session types and async
  boundary misuse.

## P1: Async, modules, and bootstrap

- [ ] Model async host ops as pending promises/jobs owned by the runtime event
  loop, not only activity counters.
- [ ] Add deterministic completion delivery for timers, microtasks, host ops,
  streams, and dynamic imports.
- [ ] Support ESM cycles with module records and live bindings instead of
  rejecting cycles during graph construction.
- [ ] Add dynamic import loading through the same capability-aware module loader
  used by static imports.
- [ ] Centralize Web API bootstrap through runtime surface specs; product crates
  should provide specs, not mutate globals ad hoc.
- [ ] Define a stable error taxonomy for load, parse, compile, permission, and
  runtime failures.

## P2: Ergonomics, performance, and docs

- [ ] Add zero-cost macros only as sugar over the static spec/builder/bootstrap
  backend.
- [ ] Add startup/bootstrap benchmarks for runtime construction, hosted module
  install, and Web API install.
- [ ] Add native call and string/value conversion microbenchmarks.
- [ ] Add docs for the public embedding API and remove stale VM limitation
  wording.
- [x] Add contributor tests that verify product crates do not depend directly on
  `otter-vm` once their migration is complete.

## First migration slices

1. [x] Hide raw VM installer fields from `HostedModule` and route construction
   through runtime-owned constructors.
2. [x] Add the hosted module builder backend in `otter-runtime`.
3. [x] Convert `otter-modules` to the runtime hosted module builder and remove its
   direct `otter-vm` dependency.
4. [x] Add the runtime global surface API and migrate `otter-web` class bootstrap to
   it.
5. [x] Add receiver-aware/native-method context and host-owned object storage.
6. [x] Replace hosted-module `Arc<Mutex<_>>` state closures with
   receiver-based host objects.
7. [x] Replace Web API `Arc<Mutex<_>>` state closures with receiver-based host
   objects.
8. [x] Move `otter-web` off direct `otter-vm` dependencies.

## Temporary bridge policy

During migration, runtime may contain hidden VM adapter entry points so existing
product crates keep building. These entry points must be documented as temporary,
must not be promoted in docs, and must be removed once the runtime-owned builder
API covers the same behavior.
