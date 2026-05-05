# Task 70 — GC track master tracker

## Status

- [ ] Phase 1 — page-based generational GC + pointer compression + card table + black alloc + DevTools snapshot + per-type migration (tasks 71–84)
  - [x] 71 — crate skeleton + ADR-0004 (closed 2026-05-02)
  - [x] 72 — core heap and handles (closed 2026-05-04)
  - [x] 73 — OOM + cap enforcement; `Runtime::max_heap_bytes` load-bearing (closed 2026-05-04)
  - [x] 74 — `GcStats` + `HeapSnapshot` + retained-size walker + `Runtime::heap_stats` / `heap_snapshot` / `force_gc` (closed 2026-05-04)
  - [x] 75 — `RuntimeState::trace_roots` + `GcTrace` stubs on every future-`Gc` type + `Runtime::force_gc` wiring + per-root smoke-test scaffold (closed 2026-05-04)
  - [x] 76 — `UpvalueCell` migrated to `Gc<UpvalueCellBody>` + `SafeTraceable` trait + `alloc_old` / `with_payload` / `read_payload` / `write_barrier_raw` GC APIs + `GcHeap` moved into `Interpreter` + `Frame::for_function_with_heap` / `Frame::build_upvalues` + `Value::trace_value_slots` for closure-spine walk + upvalue smoke test un-ignored + `counter_closure_no_leak` regression (closed 2026-05-04)
  - [x] 76A — `RuntimeCx`/`NativeCtx`, `!Send + !Sync` static assertions, trybuild compile-fail fixtures (closed 2026-05-05)
  - [x] 77 — `JsObject` → `Gc<ObjectBody>` (split 77A → 77B → 77C; closed 2026-05-05)
  - [x] 78 — `JsArray` → `Gc<ArrayBody>` + explicit heap API + dense-element cap accounting + array cycle/root regressions (closed 2026-05-05)
  - [x] 79 — `JsMap` / `JsSet` → `Gc<MapBody>` / `Gc<SetBody>` + explicit heap API + strong entry tracing + self-cycle regressions (closed 2026-05-05)
  - [x] 80 — `JsWeakMap` / `JsWeakSet` → GC-managed ephemeron tables + split mark/additional/sweep fixpoint + dead-key pruning regressions (closed 2026-05-05)
  - [x] 81 — `WeakRef` / `FinalizationRegistry` GC bodies + weak-finalization registry + microtask cleanup enqueueing + unregister/resurrection/laziness regressions (closed 2026-05-05; Test262 parity deferred)
- [ ] Runtime binding — explicit VM context + Tokio-first public handle (tasks 76A, 85)
- [ ] Workers / isolate pools (task 92)
- [ ] Compile-time GC safety hardening (task 93)
- [ ] Contributor-facing GC/VM API surface (task 94)
- [ ] Contributor book / plugin and macro guide (task 95)
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
| 76A | ✅ closed (2026-05-05) | `RuntimeCx<'rt>` / `NativeCtx<'rt>` types in `crates-next/otter-vm/src/runtime_cx.rs`; `!Send + !Sync` static assertions on `GcHeap` / `Gc<T>` / `Local<'gc, T>` / `HandleScope` / `Interpreter` / `NativeCtx<'_>`; trybuild compile-fail fixtures rejecting `Gc<T>` / `Local<'gc, T>` / `GcHeap` captured into `Send` futures (`crates-next/otter-vm/tests/compile_fail/`). Thread-default escape hatch deleted in 77C. |
| 77 | ✅ closed (2026-05-05) | `JsObject` → `Gc<ObjectBody>`; the heart of the leak surface; write barriers on every property store. Split into 77A/77B/77C. |
| 77A | ✅ closed (2026-05-04) | Body type swap + `SafeTraceable` impl + explicit-`&[mut] GcHeap` API inside `object.rs`. Workspace build expected red until 77B. |
| 77B | ✅ closed (2026-05-05) | Mechanical caller sweep across `crates-next/otter-vm/src/` — thread `&[mut] gc_heap` through every site. Workspace build green at the end. |
| 77C | ✅ closed (2026-05-05) | Un-ignore root smoke tests, add `proto_cycle_reaped` regression, delete `#[doc(hidden)]` thread-default shims from `heap.rs`, tighten 76A's third box to `[x]`. |
| 78 | ✅ closed (2026-05-05) | `JsArray` → `Gc<ArrayBody>`; write barriers on element / named-prop stores. |
| 79 | ✅ closed (2026-05-05) | `JsMap` / `JsSet` → `Gc<…>`; write barriers on entry stores. |
| 80 | ✅ closed (2026-05-05) | `WeakMap` / `WeakSet` with ephemeron fixpoint (closes "task 57" markers). |
| 81 | ✅ closed (2026-05-05; Test262 parity deferred) | `WeakRef` + `FinalizationRegistry`. |
| 82 | [82-migrate-promise-iterator-generator.md](./82-migrate-promise-iterator-generator.md) | `JsPromiseHandle::Pure`, `IteratorState`, generator state; parked frame trace bodies. |
| 83 | [83-migrate-bound-native-regexp.md](./83-migrate-bound-native-regexp.md) | `BoundFunction`, `NativeFunction`, `JsRegExp` — last `Rc`-shared variants. |
| 84 | [84-phase1-closeout-test262-array-sweep.md](./84-phase1-closeout-test262-array-sweep.md) | Phase 1 exit criteria: regression suite + cap-as-`RangeError` + `bash scripts/test262-safe.sh built-ins/Array` end-to-end on a 16 GB host. |

## Runtime / async binding

| # | File | One-line goal |
|---|------|---------------|
| 85 | [85-tokio-event-loop-runtime-handle.md](./85-tokio-event-loop-runtime-handle.md) | Tokio-first `EventLoop` trait + default `TokioEventLoop`; public `Otter` / `RuntimeHandle` are `Send + Sync`; isolate runner owns the `!Send` VM and GC. |
| 92 | [92-worker-isolates-and-structured-clone.md](./92-worker-isolates-and-structured-clone.md) | Worker isolates and isolate pools; no GC handle crosses worker boundaries; communication via structured clone / transferables. |
| 93 | [93-gc-branded-session-api.md](./93-gc-branded-session-api.md) | Compile-time-branded GC session/root/weak API inspired by Oscars/gc-arena: cross-isolate and stale-GC-context misuse should fail to compile, not rely on runtime discipline. |
| 94 | [94-gc-contributor-api-surface.md](./94-gc-contributor-api-surface.md) | Clean safe GC/VM API for engine contributors and extension authors: V8-style handle tiers, Boa-style derive ergonomics, Otter-branded safety, and narrow unsafe internals. |
| 95 | [95-contributor-book-and-extension-guides.md](./95-contributor-book-and-extension-guides.md) | mdBook contributor guide covering engine architecture, GC API, hosted modules, JS surface builders, startup performance, future plugin system, and macros. |

## Post-GC production API / startup

| # | File | One-line goal |
|---|------|---------------|
| 96 | [96-production-js-surface-builders.md](./96-production-js-surface-builders.md) | Static JS surface specs + mutator-bound builders + centralized bootstrap registry; high-level contributor API without runtime hot-path overhead. |
| 97 | [97-zero-cost-js-surface-macros.md](./97-zero-cost-js-surface-macros.md) | Macros generate static specs and normal Rust functions over task 96; no runtime registry, per-call allocation, or hidden control flow. |
| 98 | [98-startup-bootstrap-performance.md](./98-startup-bootstrap-performance.md) | Startup/first-run benchmark ratchets, bootstrap telemetry, tiered/lazy init evaluation, and startup snapshot/code-cache decision. |

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
- Task 76A's explicit-context rule is binding for tasks 77–83: no
  product-code `GcHeap::with_thread_default*` or raw thread-local heap
  lookup.
- Task 93's branded-session rule is the next hardening layer over
  task 76A: GC/worker/native APIs should move to branded `Root` /
  `Weak` / session-context shapes even when that requires breaking
  Rust API changes. The new engine is still in migration; compile-time
  safety and simpler invariants win over preserving interim APIs.
- Task 94's contributor-facing API rule is binding after it lands:
  extension/native/builtin authors should allocate, root, mutate, trace,
  and account memory through safe wrappers. Direct `RawGc`,
  `TraceTable`, manual barrier, or raw handle-table access should remain
  internal to `otter-gc` / tightly-audited VM internals.
- Balance rule: Rust safety and public extensibility are both product
  requirements. Prefer APIs that make common extension work simple and
  safe; expose low-level escape hatches only when a production use case
  cannot be served by a safe wrapper, and isolate that escape hatch
  behind explicit `unsafe` contracts and tests.
- Documentation rule: contributor-facing APIs are not complete until
  the book has buildable examples and docs for them. Task 95 is the
  home for the mdBook guide; tasks 93/94 and post-GC production API
  tasks 96-98 must update it when branded sessions, plugin APIs,
  builders, macros, bootstrap, or startup-performance workflows change.
- Write barriers wired at every pointer store **as part of the
  migration that adds the store** — not as a follow-up sweep.
- After each migration: full `cargo test --workspace` green and
  `cargo run -p otter-cli -- test --suite engine` green.
- Every PR cites the architecture-doc section it implements.
- Breaking API changes are allowed inside `crates-next/*` when they
  remove unsoundness risk, runtime-only checks, thread-local coupling,
  startup regressions, hot-path overhead, or compatibility shims. Do not
  preserve an interim API just because downstream code already adapted to
  it.
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

When all Phase 1 tasks (71–84) and blocker task 76A are ticked, leave
this tracker alive and collapse 71–84 entries into a single ✅ row pointing at the
test262 baseline snapshot.
