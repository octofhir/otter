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

For current implementation tasks, see `docs/new-engine/tasks/`.
