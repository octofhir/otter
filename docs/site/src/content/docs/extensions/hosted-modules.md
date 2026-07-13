---
title: "Hosted Modules"
---

Hosted modules expose native Rust functionality to JavaScript through the
runtime.

Use hosted modules for Otter-owned APIs such as:

- `otter:kv`;
- `otter:sql`;
- `otter:ffi`;
- standard-facing or runtime-specific modules.

Hosted modules must enforce capabilities at the Rust boundary. Do not
trust JavaScript wrappers or TypeScript declarations as the only
permission check.

The hosted module crate is `crates/otter-modules`.

## Resolution

Hosted modules are registered on the runtime builder:

```rust,ignore
let mut runtime = otter_runtime::Runtime::builder()
    .hosted_modules(otter_modules::hosted_modules().iter().copied())
    .build()?;
```

The module loader resolves registered `otter:*` specifiers directly to
their specifier string. The module graph creates a hosted module namespace
instead of reading a source file, so JavaScript can import the surface with
normal ESM syntax:

```javascript
import { openKv } from "otter:kv";
const store = openKv(":memory:");
```

Unregistered `otter:*` specifiers fail resolution.

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

## Bootstrap And Builders

The production builder/spec flow handles namespace installation. Hosted
modules should use runtime-owned specs such as `RuntimeNamespaceSpec` or the
`HostedModuleCtx` / `RuntimeObjectBuilder` API when their surface needs
capability-aware installation. Keep module registration centralized and easy
to audit. If capability enforcement or bootstrap order is delicate, prefer
explicit manual code over hiding control flow behind a macro.

Module namespaces that need runtime capabilities install through
`HostedModuleCtx::method` with `HostedNativeCall::dynamic(...)` closures that
capture owned, `Send + Sync` host data such as a cloned `CapabilitySet`.
Plain namespace exports should use `HostedModuleCtx::builtin_method`,
`HostedModuleCtx::property`, and `HostedModuleCtx::readonly_property`.
Receiver-backed resource objects should be created with
`RuntimeObjectBuilder::from_host_data(ctx, data)` and accessed through
`runtime_with_host_data` / `runtime_with_host_data_mut`. This is still a static
registration path: the module specifier list is fixed, resolution is
centralized, and no per-call metadata parser or hot-path dynamic registry is
introduced.

Macros are appropriate when they generate the same static specs and builder
calls a manual implementation would write. If a module surface needs new macro
ergonomics, add the macro over the builder API rather than bypassing it.

## Active Modules

The current active slices are:

- `otter:kv`: `openKv` / `kv`, with in-memory and file-backed JSON stores.
- `otter:sql`: `openSql` / `sql`, backed by SQLite.
- `otter:ffi`: `dlopen`, permission-checked library loading metadata.

Node compatibility is opt-in through `otter_node::NodeApiBuilderExt`:

```rust,ignore
let mut runtime = otter_runtime::Runtime::builder()
    .with_node_apis()
    .build()?;
```

The Node crate registers both `node:` and bare CommonJS aliases. Current
ecosystem-facing modules include `fs`, `path`, `url`, `util`, `util/types`,
`tty`, `events`, `buffer`, `os`, `querystring`, timers, streams, crypto, and
zlib. `node:url` supports WHATWG constructors plus file-URL conversion in
CommonJS; `pathToFileURL` and `fileURLToPath` are also named ESM exports.
`node:tty` deliberately exposes deterministic non-TTY streams and does not
open or inspect host descriptors.

The pure `path`, `url`, `util`, and TTY-shape helpers require no capability.
Resource modules still enforce deny-by-default capabilities at their native
Rust boundary (`fs` for paths and `child_process` for subprocesses).

## Node-API Addons

With `NodeApiBuilderExt` enabled, CommonJS resolution also supports native
`.node` packages. Resolution walks ancestor `node_modules` directories, reads
`package.json#main`, and probes the usual `.js`, `.cjs`, `.json`, and `.node`
entries. A native addon requires both filesystem read permission and an FFI
allowlist match for the resolved library; either capability remains denied by
default.

Otter implements the stable Node-API C ABI directly over its own VM. Addon
values are persistent-root handles rather than raw moving-GC values, and
asynchronous completion returns through the runtime microtask checkpoint. The
CLI exports the `napi_*` symbols needed by dynamically loaded addons. This path
has been exercised against a C ABI fixture, the current napi-rs Rollup native
package, the current `@swc/core` native package, and the current Lightning CSS
native package. Both symbol-based initializers and the constructor-based
`napi_module_register` ABI used by `@parcel/watcher` are recognized. The
validation includes
synchronous and asynchronous Rollup parsing, hashing, a complete
`rollup(...).generate(...)` run, and a native SWC TypeScript transform. Buffer
views and addon-reported external memory use the VM's typed-array and heap
accounting APIs rather than detached shadow storage. Lightning CSS synchronous
CSS transformation is covered; cross-thread `napi_threadsafe_function` delivery
still requires an owned runtime-message bridge and currently fails explicitly
instead of retaining a mutator-turn `NativeCtx` across threads.

Node and napi-rs themselves are not embedded. An addon must use Node-API; a
binary linked directly against V8 or private Node internals is a different ABI
and cannot be made portable by a module-loader shim.

## TypeScript Declarations

Otter does not maintain a handwritten copy of Node's declarations.
`otter-types` references `node` and depends on `@types/node` with an unpinned
range so normal package installation selects the current release. Otter-only
globals remain in Otter's declarations.

Running JavaScript or TypeScript with `otter run` does not require
`node_modules/@types/node`: declarations are static-tooling input, not runtime
modules. Editor and type-check workflows should install or acquire the official
package. A future no-`node_modules` type-check path should resolve the same
official package through the package-manager cache instead of vendoring or
forking its `.d.ts` files.

`otter:kv` and `otter:sql` have module-graph tests that import the module
specifier and execute the exported native functions.
