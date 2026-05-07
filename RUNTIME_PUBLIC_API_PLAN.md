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
- [x] Add runtime-owned builder primitives for namespaces, classes, methods,
  accessors, constants, and prototypes.
- [x] Add a host-owned object primitive so Web APIs do not use per-instance
  `Arc<Mutex<_>>` dynamic closures to hold state.
- [x] Add native method call context with receiver and constructor/new-target
  information.
- [ ] Add a Deno-style import/module-loading policy: entrypoint and statically
  analyzable local module graph loads are allowed as code loading, while
  privileged host I/O remains deny-by-default and future non-analyzable dynamic
  imports / remote imports require explicit import capabilities.
- [x] Add compile-fail tests for non-`Send`/non-`Sync` VM session types and async
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
- [x] Centralize Web API bootstrap through runtime surface specs; product crates
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
9. [x] Add runtime-owned surface helpers and migrate `otter-web` URL, Headers,
   Blob, Request, and Response class specs to them.
10. [x] Migrate `otter:kv`, `otter:sql`, and `otter:ffi` away from product-code
    bridge imports and raw hosted-call dynamic
    adapters.
11. [x] Move `HostedModuleCtx`, `HostedNativeCall`, and `GlobalClass` internals
    onto runtime-owned surface types; keep VM object/class builders contained
    in the runtime surface backend.
12. [x] Add an `otter-web` builder preset for Web API globals and enable it by
    default in the CLI without adding an `otter-runtime -> otter-web`
    dependency.

## Bridge Policy

Runtime-owned aliases may still point at the VM backend while the public value
and context facade matures, but product crates should not use VM-shaped bridge
modules or raw hosted-call adapters.

Current backend bridges:

- `RuntimeValue`, `RuntimeNativeCtx`, and static spec names are currently
  runtime-owned aliases over the VM backend. This is the accepted intermediate
  until the public value/context facade is split further.

Removed bridges:

- `otter_runtime::module_api` has been removed from the active public surface.
- `HostedNativeCall::from_raw` and `GlobalClass::from_raw` have been removed;
  runtime/product code uses `HostedNativeCall::static_fn`,
  `HostedNativeCall::dynamic`, and `GlobalClass::from_runtime`.
