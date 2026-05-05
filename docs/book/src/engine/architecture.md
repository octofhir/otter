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

Builtin and extension surfaces should install through a centralized
bootstrap registry backed by static specs and mutator-bound builders. This
keeps contributor ergonomics high while preserving write barriers,
deterministic install order, fast native-call dispatch, and startup
benchmark visibility.

Documentation for stable contributor workflows belongs in this book.
`docs/new-engine/tasks/` records implementation plans and closeout history.

For current implementation tasks, see `docs/new-engine/tasks/`.
