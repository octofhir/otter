# Task 92 — Worker isolates, structured clone, and isolate pools

## Status

- [x] worker runtime owns a separate isolate and `GcHeap` for the
      host-facing `Worker` / `OtterPool` API
- [x] `Worker` / worker-handle public API designed for host-side
      isolate execution
- [x] structured clone implemented for supported values available now
- [x] transfer-list plumbing designed for future `ArrayBuffer`,
      `MessagePort`, and stream/resource ownership
- [x] isolate pool strategy prototyped as round-robin `OtterPool`
- [x] branded GC/session constraints moved to task 93 because the
      branded `Root` / `Weak` / `GcSession` types do not exist yet
- [x] gates green for task-92 scope

## Progress Notes

- 2026-05-06: added `otter-runtime::structured_clone` with owned,
  sendable payloads and an explicit `&GcHeap` VM-to-payload clone helper
  for primitives, strings, arrays, plain enumerable objects, `Map`, and
  `Set`. Error objects clone as diagnostic payloads. The clone walker is
  depth-limited and rejects cycles / unsupported values deterministically.
  JS-visible worker message delivery and concrete transferable backing
  stores remain open.
- 2026-05-06: added host-facing `Worker` and `OtterPool` handles. Each
  worker is backed by its own `RuntimeHandle` / isolate runner, so worker
  scripts run with separate VM state and separate GC heaps. Tests cover
  concurrent Tokio multi-thread execution and pool round-robin isolation.
  This is not yet the JS `Worker` global or `MessagePort` surface.
- 2026-05-06: worker shutdown diagnostics now report queued messages,
  pending runtime work, live runtime handle references, and the future
  transferable leak count. Compile-fail coverage proves the current worker
  message boundary rejects `otter_vm::Value`, `otter_gc::Gc<T>`,
  `otter_gc::Local<'gc, T>`, and `otter_vm::NativeCtx<'_>`.
- 2026-05-06: branded `Root<'iso, T>`, `Weak<'iso, T>`, and
  `GcSession<'_, '_>` worker-boundary proof obligations were moved to
  task 93, where those types are introduced and can be compile-fail
  tested honestly.

## Goal

Prepare Otter for multi-core JS execution without weakening the GC model.
Workers are separate isolates. They communicate by structured clone,
transferables, and message ports. No `Gc<T>`, `Local<'gc, T>`, internal
`Value`, `Frame`, or runtime borrow crosses a worker boundary.

This task is not required for the first CLI release, but its constraints
must be respected by task 85's `RuntimeHandle` design.

## Source

- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)
- [`85-tokio-event-loop-runtime-handle.md`](./85-tokio-event-loop-runtime-handle.md)
- [`93-gc-branded-session-api.md`](./93-gc-branded-session-api.md)

## Scope

1. **Worker model.**
   Each worker owns:
   - `RuntimeCore`
   - `Interpreter`
   - `RuntimeState`
   - `GcHeap`
   - command/completion queues
   - its own event-loop attachment

2. **Message boundary.**
   Implement a structured clone layer for the value subset available at
   this point:
   - primitives;
   - strings;
   - arrays;
   - plain objects;
   - maps/sets after tasks 79-80;
   - errors as structured diagnostic payloads.

   Unsupported values fail with a structured clone error, not a panic.

   After task 93, the type shape should make this boundary harder to
   misuse: `Gc<T>`, `Local<'gc, T>`, `Weak<'iso, T>`,
   `Root<'iso, T>`, `NativeCtx<'_>`, and `GcSession<'_, '_>` cannot be
   sent as clone payloads. Transferables must move backing resources, not
   branded heap pointers.

3. **Transferables.**
   Design the transfer-list interface for `ArrayBuffer`, streams, and
   message ports. Implement only the pieces whose backing types already
   exist.

4. **Isolate pool.**
   Document and prototype an isolate-pool API for high-throughput server
   workloads:
   ```rust
   let pool = OtterPool::builder().workers(n).build();
   let result = pool.run_script(source).await?;
   ```
   Pool routing must preserve per-request isolation and must not share a
   heap between workers.

5. **Diagnostics.**
   Worker shutdown reports live handles, queued messages, pending host
   ops, and leaked transferables.

6. **Branded isolation.**
   If task 93 lands first, worker APIs must preserve its brand model:
   each worker creates a fresh isolate brand, worker pools cannot expose
   a common brand for multiple heaps, and any FFI-erased worker handle
   must re-enter the owning isolate before dereferencing a root or weak
   handle.

## Out of scope

- SharedArrayBuffer / Atomics multi-agent semantics.
- Parallel execution inside a single isolate.
- Web server framework integration.

## Validation gates

- [x] Compile-fail test proves worker messages cannot carry currently
  available isolate-local handles: `Gc<T>`, `Local<'gc, T>`, internal
  `Value`, or `NativeCtx<'_>`.
- [x] Extend compile-fail coverage to `Weak<'iso, T>`, `Root<'iso, T>`,
  and `GcSession<'_, '_>` moved to task 93, where those types are
  introduced.
- [x] Compile-fail test proves a root/weak handle created by one worker
  cannot be dereferenced or upgraded through another worker's branded
  session: moved to task 93.
- [x] Two workers can run scripts concurrently on a Tokio multi-thread
  runtime without sharing heap state.
- [x] Structured clone cycle handling is deterministic and depth-limited.
- [x] Unsupported clone values return a structured error.
- [x] Worker shutdown leak diagnostics are covered.
- [x] `cargo test -p otter-runtime` and `cargo test -p otter-vm` green.

## Closing

Task 92 is closed for the currently implementable worker/isolate and
structured-clone scope. The branded-session follow-up is tracked by
task 93.
