# Hosted Modules

Hosted modules expose native Rust functionality to JavaScript through the
runtime.

Use hosted modules for Otter-owned APIs such as:

- `otter:kv`;
- `otter:sql`;
- `otter:ffi`;
- future standard-facing or runtime-specific modules.

Hosted modules must enforce capabilities at the Rust boundary. Do not
trust JavaScript wrappers or TypeScript declarations as the only
permission check.

## File Layout

Use the repository naming convention:

| File | Purpose |
| ---- | ------- |
| `module_ext.rs` | Rust implementation of native functions |
| `module.js` | JavaScript shim, wrapper, or polyfill |

Rust code owns permissions, resource opening, async dispatch, GC rooting,
and external-memory accounting. JavaScript shims may normalize arguments
or provide ergonomic exports, but they must not be the only enforcement
layer.

## Native Boundary

Hosted module native functions should:

1. validate arguments on the isolate thread;
2. check capabilities before opening host resources;
3. allocate through `NativeCtx`;
4. store persistent JS references as branded roots, not raw `Gc<T>`;
5. account host buffers with `ExternalMemory`;
6. for async work, copy owned host data into the future and post an owned
   completion back to the isolate.

Do not move VM values, handles, contexts, frames, or handle scopes into
Rust futures.

## Bootstrap

Task 96 will provide the production builder/spec flow for hosted module
namespace installation. Until then, keep module registration manual,
centralized, and easy to audit. If capability enforcement or bootstrap
order is delicate, prefer explicit manual code over hiding control flow
behind a macro.

Task 97 may add hosted-module macros later, but those macros must generate
Task 96 specs and ordinary Rust functions. They must not invent a separate
loader registry.
