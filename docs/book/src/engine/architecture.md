# Engine Architecture

The new engine is split into focused crates:

- `otter-gc`: page-based generational tracing GC;
- `otter-bytecode`: bytecode representation and disassembly;
- `otter-syntax` / `otter-compiler`: frontend and lowering;
- `otter-vm`: interpreter, object model, intrinsics, and runtime state;
- `otter-runtime`: embedding/runtime surface;
- `otter-cli`: command-line entry point.

One JavaScript isolate owns one VM, one runtime state, and one GC heap.
Async and worker APIs must preserve that boundary. Values move between
workers through structured clone or transferables, not raw GC handles.

## Pipeline

Source flows through the active frontend and VM stack:

```text
source file / source string
  -> otter-syntax / otter-compiler
  -> otter-bytecode
  -> otter-vm Interpreter
  -> otter-runtime facade / CLI / embedder
```

The compiler should use AST APIs for JS/TS analysis and transforms.
Bytecode and VM changes should keep debug/disassembly output stable enough
for trace and Test262 triage.

## Runtime Boundary

`RuntimeCx<'_>` is the internal VM mutator context. It carries the active
interpreter/heap borrow so VM helpers cannot silently use thread-local heap
state. `NativeCtx<'_>` is the public native-binding view exposed to
builtin and extension authors.

Both contexts are isolate-local and must not cross `.await`, worker
boundaries, runtime inbox messages, or host-operation futures. Async work
copies owned host data out, then re-enters the isolate later with an owned
completion.

## GC And Handles

The GC is page-based, moving, generational, and isolate-local. Normal
engine work uses safe handles and context helpers:

- stack-scoped `Local<'gc, T>` for temporary roots;
- `EscapableHandleScope` when one local must leave a nested scope;
- branded `Root<'iso, T>` for persistent isolate-owned references;
- branded `Weak<'iso, T>` for weak references upgraded only through a
  matching session;
- `NativeCtx::record_write` / `GcHeap::record_write` for stores;
- `ExternalMemory` for native/off-object bytes.

Raw collector types live behind `otter_gc::raw` for audited adapters.
They are not a contributor API.

## Modules And Permissions

Module loading, hosted modules, Web APIs, and Node-style surfaces must
enforce capabilities at the Rust boundary. Type declarations and JS shims
are useful ergonomics, not security boundaries. Capability checks should
happen before host work is started and before native resources are opened.

## Bootstrap

Builtin and extension surfaces should install through a centralized
bootstrap registry backed by static specs and mutator-bound builders. This
keeps contributor ergonomics high while preserving write barriers,
deterministic install order, fast native-call dispatch, and startup
benchmark visibility.

Task 96 owns the production builder/backend. Until it lands, do not
promise builder or macro APIs as stable; write manual native/bootstrap code
using the explicit context APIs and keep install order obvious.

## Debug Workflows

Use the existing machine-readable trace and profiling outputs when
debugging engine behavior:

- VM instruction trace for stuck bytecode loops;
- timeout dumps for hangs;
- Chrome/Perfetto async trace for host-op scheduling;
- Chrome/V8 `.cpuprofile` plus folded stacks for CPU work.

New debug/profiling features should stay default-off and should use
standard output formats where possible.

Documentation for stable contributor workflows belongs in this book.
`docs/new-engine/tasks/` records implementation plans and closeout history.
