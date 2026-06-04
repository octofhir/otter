---
title: "Extensions Overview"
---

Otter's extension model is layered:

1. hosted modules inside the workspace;
2. native bindings compiled with the engine;
3. source-level plugin packages when a stable extension crate exists;
4. ABI/FFI plugins only with explicit versioning and ownership rules.

All layers must preserve the same runtime rules:

- permissions are deny-by-default;
- no raw GC handle crosses isolate or worker boundaries;
- persistent JS-visible state uses `Root`;
- weak handles upgrade only through a matching context;
- external memory is accounted;
- async work hops back to the isolate before touching VM state.

JavaScript-visible surfaces should use the production spec/builder flow:

- static specs declare names, arity, attributes, and native targets;
- builders install specs through explicit `RuntimeCx` / `NativeCtx`;
- centralized bootstrap owns global/prototype/module install order;
- macros, when available, generate the same static specs rather than a
  separate runtime registry.

The first stable builder/spec backend lives in `otter-vm::js_surface` plus
the centralized `otter-vm::bootstrap` registry. New JS-visible surfaces
should use that path unless capability checks or delicate install order
require a small manual installer that still calls the same builders.

Breaking changes to extension APIs are allowed while the active engine API is
pre-stable if they improve safety, startup, or steady-state
performance.

Plugin details stay design-only until the API is stable enough to document
here fully.

## Extension Checklist

For any new JS-visible API:

- choose the active `crates/*` crate that owns the behavior;
- decide whether the surface is a global, builtin class, namespace,
  hosted module, or runtime-only helper;
- enforce permissions at the Rust boundary;
- allocate, root, mutate, and account memory through context APIs;
- add runtime tests and TypeScript declarations when the surface is
  public;
- add docs here when the workflow is stable.
