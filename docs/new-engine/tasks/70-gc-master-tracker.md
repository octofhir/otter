# Task 70 — GC track master tracker

## Status

- [ ] Phase 1 — page-based generational GC + pointer compression + card table + black alloc + DevTools snapshot + per-type migration (tasks 71–84)
  - [x] 71 — crate skeleton + ADR-0004 (closed 2026-05-02)
  - [x] 72 — core heap and handles (closed 2026-05-04)
  - [x] 73 — OOM + cap enforcement; `Runtime::max_heap_bytes` load-bearing (closed 2026-05-04)
- [ ] Phase 2 — incremental marking + concurrent sweeping + pretenuring (task 86)
- [ ] Phase 3 — Mark-Compact + memory reducer + sticky mark-bit (tasks 88, 89, 90)
- [ ] Phase 4 (deferred indefinitely) — concurrent marking + parallel scavenge (task 87)

## Goal

Retire `Rc<RefCell<…>>` from every heap-shared type in
`crates-next/otter-vm` by introducing a **production-grade, V8/JSC-shaped
generational tracing GC**. Every `Rc<RefCell<T>>` in the value model
becomes a `Gc<T>` handle backed by a single page-based `GcHeap`. The
leaks blocking the test sweep are a **symptom**; the **cause** is `Rc`'s
structural inability to break cycles.

## Strategy

We do **not** start with a deliberately-worse handle-table interim.
The legacy `crates/otter-gc/` is V8/JSC-shaped 2026 code (page-based
heap, semispace scavenger, tri-color marking, write barriers, atomic
header) — read it as a design reference and rewrite under the
new-engine conventions inside `crates-next/otter-gc/`. **No path-dep
on `crates/*`** ([`tasks/README.md`](./README.md) §Working rules 1–2;
ADR-0001 keeps `crates/*` excluded from the workspace). One migration
sweep, one write-barrier audit — not two.

## Source of truth

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md). Every
task in this track cites a section of that doc as its design source.

## Phase 1 — page-based generational GC

Pick the next unticked task and ship it end-to-end. Order matters —
later tasks assume earlier ones have landed.

| # | File | One-line goal |
|---|------|---------------|
| 71 | ✅ closed (2026-05-02) | New `crates-next/otter-gc` crate; ADR-0004 amends ADR-0001 §5 to lift `forbid(unsafe_code)` for this crate only. |
| 72 | ✅ closed (2026-05-04) | Page heap: `GcHeader`, `Page`, `NewSpace`/`OldSpace`/`LargeObjectSpace`, scavenger, marking, barriers, trace table, `Gc<T>` + `Local<'gc, T>` + `HandleScope<'gc>`. |
| 73 | ✅ closed (2026-05-04) | `OutOfMemory::HeapCapExceeded` payload + `GcHeap::with_max_heap_bytes` + tracked / reserved bytes + retry-once-then-fail emergency-collect path; `Runtime::max_heap_bytes` plumbed end-to-end with `From<otter_gc::OutOfMemory>` mapping. |
| 74 | [74-gc-stats-and-snapshot.md](./74-gc-stats-and-snapshot.md) | `GcStats`, `HeapSnapshot`, retained-size walker, `Runtime::heap_stats()`. |
| 75 | [75-gc-root-enumeration.md](./75-gc-root-enumeration.md) | `RuntimeState::trace_roots`: frames, microtask queue, module envs, dynamic-import host, symbol registry; smoke tests. |
| 76 | [76-migrate-upvalue-cell.md](./76-migrate-upvalue-cell.md) | `UpvalueCell` from `Rc<RefCell<Value>>` → `Gc<UpvalueCellBody>` + write-barrier wiring on every upvalue store. |
| 77 | [77-migrate-jsobject.md](./77-migrate-jsobject.md) | `JsObject` → `Gc<ObjectBody>`; the heart of the leak surface; write barriers on every property store. |
| 78 | [78-migrate-jsarray.md](./78-migrate-jsarray.md) | `JsArray` → `Gc<ArrayBody>`; write barriers on element / named-prop stores. |
| 79 | [79-migrate-jsmap-jsset.md](./79-migrate-jsmap-jsset.md) | `JsMap` / `JsSet` → `Gc<…>`; write barriers on entry stores. |
| 80 | [80-migrate-weakmap-weakset-ephemerons.md](./80-migrate-weakmap-weakset-ephemerons.md) | `WeakMap` / `WeakSet` with ephemeron fixpoint (closes "task 57" markers). |
| 81 | [81-weakref-finalization-registry.md](./81-weakref-finalization-registry.md) | `WeakRef` + `FinalizationRegistry`. |
| 82 | [82-migrate-promise-iterator-generator.md](./82-migrate-promise-iterator-generator.md) | `JsPromiseHandle::Pure`, `IteratorState`, generator state; parked frame trace bodies. |
| 83 | [83-migrate-bound-native-regexp.md](./83-migrate-bound-native-regexp.md) | `BoundFunction`, `NativeFunction`, `JsRegExp` — last `Rc`-shared variants. |
| 84 | [84-phase1-closeout-test262-array-sweep.md](./84-phase1-closeout-test262-array-sweep.md) | Phase 1 exit criteria: regression suite + cap-as-`RangeError` + `bash scripts/test262-safe.sh built-ins/Array` end-to-end on a 16 GB host. |

## Phase 2 — incremental marking + concurrent sweep + pretenuring

| # | File | One-line goal |
|---|------|---------------|
| 86 | [86-gc-incremental-marking.md](./86-gc-incremental-marking.md) | Incremental marking driven from back-edge + concurrent old-gen sweeping on a background thread + incremental sweeping for foreground complement + allocation-site pretenuring. Phase-1 barriers go load-bearing — no new audit sweep. |

## Phase 3 — Mark-Compact + idle GC + sticky mark-bit

| # | File | One-line goal |
|---|------|---------------|
| 88 | [88-gc-mark-compact.md](./88-gc-mark-compact.md) | Sliding compactor for old-gen pages crossing fragmentation threshold. Reclaims free-list slack; matches V8 since 2014. |
| 89 | [89-gc-memory-reducer-idle-gc.md](./89-gc-memory-reducer-idle-gc.md) | `Runtime::notify_idle(deadline)` + memory-reducer state machine. Proactive GC on idle; matches V8 MemoryReducer. |
| 90 | [90-gc-sticky-mark-bit.md](./90-gc-sticky-mark-bit.md) | Sticky-mark-bit minor cycles: keep old-gen mark bits across cycles, only re-trace newly-allocated / dirtied slots. Big throughput win on long-running steady-state workloads. |

## Phase 4 — deferred indefinitely

| # | File | One-line goal |
|---|------|---------------|
| 87 | [87-gc-concurrent-marking.md](./87-gc-concurrent-marking.md) | Concurrent marking + parallel scavenge. Single-threaded isolate makes this expensive in complexity for marginal pause-time win; pick up only when production embedders demand it. |

## Cross-cutting infrastructure (parallel to all phases)

| # | File | One-line goal |
|---|------|---------------|
| 91 | [91-gc-bench-and-soak-infra.md](./91-gc-bench-and-soak-infra.md) | Criterion benches + cargo-fuzz corpus + 24 h soak runner + miri/asan/lsan/tsan CI matrix + V8-parity benchmark suite. **Required for any production-grade exit gate to be verifiable.** |

## Working rules for this track

- One `Rc<RefCell<…>>` removed per migration task. No bundling.
- Write barriers wired at every pointer store **as part of the
  migration that adds the store** — not as a follow-up sweep.
- After each migration: full `cargo test --workspace` green and
  `cargo run -p otter-cli -- test --suite engine` green.
- Every PR cites the architecture-doc section it implements.
- Every task ends with the gates in [Closing a task](./README.md#closing-a-task).
- `unsafe` is permitted **only** in `crates-next/otter-gc/`; every
  other `crates-next/*` crate keeps `#![forbid(unsafe_code)]`.
- Hygiene per ADR-0004: `// SAFETY:` comments, `# Safety` on
  unsafe-fn docstrings, miri test for non-trivial unsafe blocks.

## What this track is *not*

- A path-dep on `crates/otter-gc/`. The legacy crate is design
  reference only.
- A two-stage backend swap (handle-table → page-heap). One backend,
  page-based, from day 1.
- A V8-evolution rewalk. The legacy crate is the 2026 state-of-the-art
  we'd derive after re-walking V8 1997→2026; we skip the walk.

## Closing this tracker

When all Phase 1 tasks (71–84) are ticked, leave this tracker alive
and collapse 71–84 entries into a single ✅ row pointing at the
test262 baseline snapshot.
