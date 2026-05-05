# Macro Overview

Otter uses macros to keep JavaScript-visible API declarations concise
while preserving explicit names, arity, and install targets.

Canonical active macros:

- `#[js_class]` for constructor-backed JavaScript classes;
- `#[js_namespace]` for namespace-style objects;
- `#[dive]` for single native bindings;
- `#[dive(deep)]` for async native bindings;
- `raft!` for grouped bindings;
- `burrow!` for host-owned object surfaces;
- `lodge!` for hosted module declarations.

Future GC work may add a `#[derive(GcTrace)]` or equivalent field-based
trace macro. Its job will be to make traced and skipped fields explicit
without requiring normal contributors to write unsafe trace code.

Prefer manual code when a macro would hide capability enforcement,
bootstrap order, or important control flow.
