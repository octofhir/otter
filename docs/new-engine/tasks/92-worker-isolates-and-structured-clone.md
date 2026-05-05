# Task 92 — Worker isolates, structured clone, and isolate pools

## Status

- [ ] worker runtime owns a separate isolate and `GcHeap`
- [ ] `Worker` / worker-handle public API designed
- [ ] structured clone implemented for supported values
- [ ] transfer-list plumbing designed for future `ArrayBuffer`
- [ ] isolate pool strategy documented
- [ ] branded GC/session constraints from task 93 are respected at the
      worker boundary
- [ ] gates green

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

- [ ] Compile-fail test proves worker messages cannot carry `Gc<T>`,
  `Local<'gc, T>`, internal `Value`, `Weak<'iso, T>`,
  `Root<'iso, T>`, `GcSession<'_, '_>`, or `NativeCtx<'_>`.
- [ ] Compile-fail test proves a root/weak handle created by one worker
  cannot be dereferenced or upgraded through another worker's branded
  session.
- [ ] Two workers can run scripts concurrently on a Tokio multi-thread
  runtime without sharing heap state.
- [ ] Structured clone cycle handling is deterministic and depth-limited.
- [ ] Unsupported clone values return a structured error.
- [ ] Worker shutdown leak diagnostics are covered.
- [ ] `cargo test -p otter-runtime -p otter-vm` green.

## Closing

Tick task 92 in [70-gc-master-tracker.md](./70-gc-master-tracker.md) and
update ADR-0005 if worker API names differ from this task.
