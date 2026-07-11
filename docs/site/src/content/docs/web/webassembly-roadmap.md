---
title: "WebAssembly: Status & Roadmap"
description: "What the WebAssembly JS API supports today (wasmtime backend) and the remaining runtime-integration features."
---

Otter's `WebAssembly` support runs on the [wasmtime](https://wasmtime.dev) engine
(the only pure-Rust WebAssembly runtime with full Wasm 3.0 support: exception
handling and reference types). Deno and Bun inherit WebAssembly from their JS
engines (V8 / JavaScriptCore, both C++); Otter has its own VM, so it embeds
wasmtime directly.

## Supported today

The full `WebAssembly.*` JS API is implemented and tested end to end:

| Surface | Status |
| --- | --- |
| `WebAssembly.validate` / `compile` / `instantiate` (+ `compileStreaming` / `instantiateStreaming`) | ✅ |
| `WebAssembly.Module` / `Instance` | ✅ |
| `WebAssembly.Memory` / `Table` / `Global` | ✅ |
| `WebAssembly.CompileError` / `LinkError` / `RuntimeError` | ✅ |
| `WebAssembly.Tag` / `Exception` / `JSTag` (exception handling) | ✅ |
| Cross-store imports (a standalone `Memory` / `Table` / `Global` linked into any instance) | ✅ |
| `i64` params/results as JS `BigInt` (`BigInt.asIntN(64)` lowering) | ✅ |
| `externref` — JS values round-trip through wasm by identity | ✅ |
| JS function imports (re-enter the VM) and exported functions | ✅ |
| Tagged exceptions crossing the JS↔wasm boundary in both directions | ✅ |

### Implementation notes

- One shared `Engine` + `Store` per realm, so imports link across objects. It is
  cached on a hidden global carrier.
- Because wasmtime holds an exclusive `&mut Store` during a call, a JS import that
  re-enters and calls another export raises a `RuntimeError` rather than
  deadlocking. V8/Deno allow nested calls; this is a documented boundary.

## Remaining / potential features

These go beyond the WebAssembly JS API into runtime integration. Each is a
sizeable, standalone piece of work.

### 1. Zero-copy `Memory.buffer`

Today `Memory.buffer` returns a fresh `ArrayBuffer` snapshot on each read: a wasm
write is visible after a call, but the buffer object identity is not stable and
reads copy. A spec-faithful `Memory.buffer` is a single, live `ArrayBuffer`
backed by wasmtime's linear memory.

- **Needs:** VM support for an `ArrayBuffer` whose backing store is external
  (borrowed) memory rather than an owned VM allocation.
- **Size:** large — a core change to the VM's `ArrayBuffer` model.

### 2. ES module integration — `import x from "./m.wasm"`

Import a `.wasm` file directly as an ES module (the WebAssembly/ESM integration
proposal, shipped in Deno 2.1), with the module's exports as the module namespace
and its imports resolved from other modules.

- **Needs:** the runtime module loader to detect `.wasm`, compile + instantiate
  it, and expose exports as ESM bindings; wasm import section resolved through the
  module graph / import maps.
- **Size:** large — module-loader integration.

### 3. WASI

Run WASI modules (`wasip1` via a `node:wasi`-style surface, and/or `wasip2`
components), providing the system interface — filesystem, clock, random,
args/env — under Otter's capability model.

- **Needs:** the `wasmtime-wasi` integration plus a JS-facing surface, gated by the
  existing deny-by-default capabilities (`fs_read` / `fs_write` / `env` / …).
- **Size:** very large — a separate specification surface.

## References

- [WebAssembly JS API](https://webassembly.github.io/spec/js-api/)
- [Exception handling JS API](https://webassembly.github.io/exception-handling/js-api/)
- [WebAssembly/ES Module Integration](https://github.com/WebAssembly/esm-integration)
- [Deno 2.1: Wasm imports](https://deno.com/blog/v2.1)
