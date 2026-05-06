# Task 93 — Compile-time-branded GC session API

## Status

- [x] design note added to `docs/new-engine/gc-architecture.md`
- [x] experimental `GcSession<'iso, 'gc>` / `MutationSession<'iso, 'gc>`
      API sketched behind a non-default feature or internal module
- [x] persistent `Root<'iso, T>` wrapper specified over `GlobalHandle<T>`
- [x] weak upgrade requires a matching session/context
- [x] compile-fail tests reject cross-isolate roots, weak upgrades,
      worker messages, and native closures
- [x] task-92 worker follow-up covered: `Root<'iso, T>`,
      `Weak<'iso, T>`, and `GcSession<'_, '_>` cannot cross worker
      boundaries or be dereferenced through another worker's brand
- [x] migration guidance written for tasks 85, 92, and future FFI work
- [x] gates green; broad persistent-handle audit closed

## Goal

Raise GC safety from "explicit runtime context by convention" to
"wrong isolate / stale mutator turn is a compile error" wherever Rust's
type system can carry the proof.

Task 76A already removed thread-default heap lookup and made VM/native
entry points carry `RuntimeCx<'rt>` / `NativeCtx<'rt>`. That prevents a
large class of hidden-runtime bugs, but `Gc<T>` / `Weak` / persistent
handles are still not branded by the isolate that owns them. Backwards
compatibility for these interim Rust APIs is not a constraint: this task
may break VM/runtime/native signatures if the result makes misuse
unrepresentable. It adds an Oscars/gc-arena-style branded context layer
on top of the current V8/JSC-shaped collector:

- one fresh `'iso` brand per isolate / `GcHeap`;
- one short `'gc` brand per mutator turn / handle scope;
- `Weak` upgrade only through a matching context;
- persistent roots tied to the owning isolate, not to thread-local state;
- compile-fail tests for cross-isolate, worker, async, and FFI misuse.

Task 92 already closed the currently available worker boundary:
structured-clone payloads are owned/sendable, worker isolates do not
share VM/GC state, and compile-fail fixtures reject `Gc<T>`,
`Local<'gc, T>`, internal `Value`, and `NativeCtx<'_>` as worker
messages. This task owns the remaining branded worker proof obligations
because it introduces `Root<'iso, T>`, `Weak<'iso, T>`, and
`GcSession<'iso, 'gc>`.

The collector backend stays Otter's page-based generational heap. Do not
replace it with Oscars' mark-sweep arena prototype.

## Source

- [`70-gc-master-tracker.md`](./70-gc-master-tracker.md) runtime /
  async binding section.
- [`76a-runtime-binding-explicit-context.md`](./76a-runtime-binding-explicit-context.md).
- [`92-worker-isolates-and-structured-clone.md`](./92-worker-isolates-and-structured-clone.md).
- [`94-gc-contributor-api-surface.md`](./94-gc-contributor-api-surface.md).
- Research input: Boa Oscars API notes (`Gc<'gc, T>`,
  `WeakGc<'id, T>`, `Root<'id, T>`, `MutationContext<'id, 'gc>`,
  sentinel root list, and compile-time cross-context rejection).

## Design Targets

### 93.1 — Branded isolate/session shape

Sketch the target API and migrate call sites directly when doing so
removes ambiguity. A temporary adapter is acceptable only if it keeps
the same invariants and has a named deletion task.

```rust
pub struct IsolateBrand<'iso> {
    _marker: PhantomData<fn(&'iso mut ())>,
}

pub struct GcSession<'iso, 'gc> {
    heap: &'gc mut GcHeap,
    _iso: PhantomData<fn(&'iso mut ())>,
}

pub struct Root<'iso, T: SafeTraceable> {
    inner: GlobalHandle<T>,
    _iso: PhantomData<fn(&'iso mut ())>,
}

pub struct Weak<'iso, T: SafeTraceable> {
    raw: RawWeak,
    _iso: PhantomData<fn(&'iso mut T)>,
}
```

The exact names can change. The invariant cannot: a root or weak handle
created by isolate A must not be usable with isolate B's session.

### 93.2 — Persistent roots over `GlobalHandle`

`Root<'iso, T>` is the public persistent-root shape for embedders,
native modules, async host operations, timers, and worker handles. It
wraps the existing moving-GC-compatible global handle implementation
instead of replacing it.

Required behavior:

- `Root::get(&GcSession<'iso, '_>) -> Gc<T>` only works with the same
  isolate brand.
- `Drop` unregisters the global handle exactly once.
- runtime-drop diagnostics report leaked roots by allocation site/type.
- no `Rc`, thread-local lookup, or implicit current-heap dependency.

### 93.3 — Weak handles require matching context

Weak handles must not expose a context-free upgrade API. The target
shape is:

```rust
impl<'iso, T: SafeTraceable> Weak<'iso, T> {
    pub fn upgrade<'gc>(&self, cx: &GcSession<'iso, 'gc>) -> Option<Gc<T>>;
}
```

This mirrors the useful part of Oscars: stale/cross-context weak upgrade
is rejected by the type checker. The implementation still uses Otter's
weak/ephemeron registry and moving-GC forwarding rules.

### 93.4 — Native and async boundaries

Apply the branded API where mistakes are expensive:

- native closures that store handles beyond the call must store
  `Root<'iso, T>`, never `Gc<T>` or `Local<'gc, T>`;
- async host operations may carry `Root<'iso, T>` through the command
  queue, but may only dereference it on the isolate runner with a
  matching `GcSession`;
- workers and isolate pools must not expose a way to mix brands;
- FFI adapters must erase brands only at the outermost unsafe boundary
  and immediately revalidate/rebrand before touching the heap.

### 93.5 — Compile-fail suite

Add trybuild fixtures that prove:

- a `Root` from isolate A cannot be read through isolate B;
- a `Weak` from isolate A cannot be upgraded through isolate B;
- `Gc<T>` / `Local<'gc, T>` cannot be stored in a `'static + Send`
  future or worker message;
- `NativeCtx<'_>` / `GcSession<'_, '_>` cannot cross `.await`;
- FFI-erased handles cannot call `Root::get` without re-entering a
  branded isolate context.

## Out of scope

- Preserving the current `Gc<T>` / `GlobalHandle<T>` / native-context
  API shape for compatibility. Breaking changes are expected when they
  reduce runtime checks or make wrong-isolate use impossible.
- A single giant mechanical rewrite with no green checkpoints. Break
  APIs as needed, but land in reviewable vertical slices with tests.
- Replacing handle scopes with a non-moving arena-root model.
- Supporting multiple collector backends through a common trait. That
  was an Oscars research question, not an Otter production requirement.
- Making the collector `Send` / `Sync`. One JS isolate still has one
  mutator.

## Validation gates

- [x] `cargo test -p otter-vm --test compile_fail` covers every misuse
  category in 93.5.
- [x] `cargo test -p otter-gc -p otter-vm -p otter-runtime` green.
- [x] No public API stores `Gc<T>` where `Root<'iso, T>` is required
  for persistence across a safepoint, async boundary, or worker queue.
- [x] No product-code thread-local heap lookup is reintroduced.
- [x] Worker task 92 gates still hold after branded roots land.
- [x] Worker-boundary compile-fail tests cover `Root<'iso, T>`,
  `Weak<'iso, T>`, and `GcSession<'_, '_>` after those types exist.

## Migration guidance

- Task 85 / runtime async boundary: public `RuntimeHandle` remains the
  sendable boundary. Branded `GcSession<'iso, 'gc>` must stay on the
  isolate runner and must not cross `.await`; async host operations that
  eventually need GC access must re-enter the owning isolate before
  dereferencing roots.
- Task 92 / workers: worker messages stay structured-clone payloads or
  transfer-list metadata. `Root<'iso, T>`, `Weak<'iso, T>`, and
  `GcSession<'_, '_>` are rejected at the worker message boundary.
- Future FFI work: any brand erasure belongs only at the unsafe outer FFI
  boundary. The adapter must immediately re-enter the owning isolate and
  recover a matching branded session before calling `Root::get` or
  `Weak::upgrade`.

## Progress Notes

- 2026-05-06: added `otter_gc::branded` with invariant isolate brands,
  `GcSession` / `MutationSession`, `Root` over `GlobalHandle`, and
  `Weak` with context-required upgrade. The first slice is an
  experimental API layer over the existing collector; broad VM/runtime
  migration to require `Root<'iso, T>` for every persistent handle is
  still open.
- 2026-05-06: added trybuild coverage for cross-isolate root reads,
  cross-isolate weak upgrades, `GcSession` across async send futures,
  native closures capturing branded roots, and task-92 worker messages
  carrying branded root/weak/session values.
- 2026-05-06: required validation is green for the branded API slice:
  `cargo test -p otter-vm --test compile_fail`,
  `cargo test -p otter-runtime --test compile_fail`,
  `cargo test -p otter-gc -p otter-vm -p otter-runtime`,
  workspace clippy, workspace tests, engine suite, and CLI async smokes.
  Task 93 remains open because active public VM/runtime APIs have not
  yet been migrated/audited to require `Root<'iso, T>` for every
  persistent handle across safepoints, async boundaries, worker queues,
  native callbacks, timers, and future FFI.
- 2026-05-06: migrated one persistent-root entry point: external code
  can no longer create unbranded persistent handles through
  `GcHeap::create_global` / `GlobalHandleTable::create`. The card-table
  regression now holds its cross-safepoint test root through
  `GcSession::root`, and trybuild rejects direct unbranded
  `create_global` usage from outside `otter-gc`. This is a narrow
  slice toward the broad persistent-handle gate, not the final audit.
- 2026-05-06: hid the remaining unbranded persistent-handle surface:
  `GlobalHandle`, `GlobalHandleTable`, and the raw global-handle-table
  pointer accessor are no longer public API. External code cannot name
  or construct unbranded persistent handles; `Root<'iso, T>` is the
  exposed persistent-root shape. Added trybuild coverage for the public
  type name rejection. Task 93 remains open for the wider VM/runtime
  persistence audit.
- 2026-05-06: removed the VM's public cross-thread microtask inbox
  skeleton. `Microtask` records carry `Value` and parked `Frame` state,
  so async hosts must send owned runtime messages and re-enter the
  owning isolate before enqueueing isolate-local microtasks. The queue
  now exposes only mutator-thread enqueue/drain operations, and a
  static assertion keeps `Microtask` `!Send + !Sync`. Task 93 remains
  open for the rest of the public/runtime/native/timer persistence
  audit.
- 2026-05-06: completed the broad persistence audit for active
  `crates-next/*` runtime/native/async/timer/worker-adjacent surfaces.
  Classification:
  - Runtime async/timer/worker APIs (`RuntimeHandle`, `EventLoop`,
    `TimerRequest`, `Worker`, `OtterPool`) carry only owned source
    bundles, strings, tokens, counters, structured-clone payloads, and
    public `ExecutionResult` strings across sendable queues.
  - Structured clone accepts internal `Value` only in crate-visible
    isolate-side clone helpers and emits owned `StructuredCloneValue`;
    worker compile-fail fixtures reject `Value`, `Gc<T>`,
    `Local<'gc, T>`, `NativeCtx<'_>`, `Root<'iso, T>`,
    `Weak<'iso, T>`, and `GcSession<'_, '_>`.
  - `Microtask`, parked `Frame`, generator state, promise reactions,
    iterators, objects, arrays, maps/sets, weak refs, and
    finalization registries are persistent but isolate-local and
    traced through their owning GC bodies or `MicrotaskQueue`; they
    remain `!Send + !Sync`.
  - Public runtime raw-heap access was removed so embedders cannot
    allocate raw `Gc<T>` through `Runtime` and hold it across later
    runtime safepoints. Trybuild now rejects `Runtime::gc_heap_mut`.
  - Public native constructors now require `Send + Sync` Rust call
    closures and pass traced captures as an explicit slice at call
    time, so external native closures cannot hide `Gc<T>` / `Value`
    captures in long-lived payloads. VM-internal promise/proxy/eval
    helpers use crate-visible unchecked constructors with explicit
    captures/trace hooks.
  - `array::with_elements_mut` is no longer public and now
    conservatively fires write barriers for all GC-bearing elements
    left after the mutation.
  No remaining task-93 blocker was found in the active
  runtime/native/async/timer/worker public boundary. Lower-level
  contributor ergonomics and any future public replacement for raw VM
  heap access belong to task 94.

## Closing

Task 93 is closed in
[70-gc-master-tracker.md](./70-gc-master-tracker.md). Task 85 / task
92 API names did not need branded renames in this slice.
