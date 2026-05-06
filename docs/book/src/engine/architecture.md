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

## Async-First Runtime

The active runtime model is async-first. `Otter` is the public
async-capable facade over `RuntimeHandle` and the isolate runner. CLI
execution runs from an async `main` and awaits `Otter` execution directly;
embedding entry points may expose async or sync caller ergonomics, but
observable JavaScript semantics still converge on the same async-capable
runtime machinery.

Blocking APIs are convenience adapters. They may block the caller while the
same async-capable runtime handle drives the isolate, but they must not grow
a second sync-only engine path that bypasses timers, host ops, workers,
module loading, or future async Web APIs.

`Runtime` remains the local isolate layer for tests, compile/check/dump
workflows, and low-level embedders that deliberately drive the VM in-process.
It is not a separate product runtime with different semantics. If behavior
is observable from JavaScript, the `Otter`/`RuntimeHandle` path and the
local `Runtime` path must converge on the same VM/runtime state machinery.

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

The registry lives in `otter-vm::bootstrap` as a static ordered slice of
install entries. Each entry declares a global name, required bootstrap
feature bits, and a plain installer function. Installers receive an
explicit `&mut GcHeap` plus `globalThis`; migrated surfaces use
`otter-vm::js_surface` specs/builders, and unmigrated globals remain
small placeholders until their own slices land.

The first migrated namespace is `Math`. Direct `Math.<fn>(...)` syntax
still uses the existing bytecode fast path, while observable property
reads and extracted method calls use the real namespace object installed
from `math::MATH_SPEC`.

Default-off bootstrap telemetry is available for benchmark runs. The plain
runtime construction path does not maintain telemetry counters; benches can
call the instrumented bootstrap entry point to capture install counts, GC
allocation deltas, duplicate-name validation, and per-entry timing.

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
Historical task and ADR files are not part of the living contributor docs.
