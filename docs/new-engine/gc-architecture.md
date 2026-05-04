# Otter New-Engine GC — Architecture Plan

- **Status:** draft (architectural plan, pre-implementation)
- **Date:** 2026-05-01
- **Scope:** `crates-next/*` (new engine) — *not* legacy `crates/*`
- **Related:**
  [ADR-0001](./adr/0001-staging-directory.md) (must be amended; see §11),
  [`docs/gc-migration-plan.md`](../gc-migration-plan.md) (legacy stack),
  [`PRODUCTION_READINESS_PLAN.md`](../../PRODUCTION_READINESS_PLAN.md) §3.2.

> This document defines an architecture, not an implementation. It
> deliberately stops at type signatures and pseudocode. The goal is
> that an implementation track can land Phase 1 in 1–2 weeks and
> unblock the test sweep without rewriting public API.

---

## 0. TL;DR

- **Primary goal:** *retire `Rc<RefCell<…>>` from every heap-shared
  type in `crates-next/otter-vm`*. The leaks the user is hitting are a
  symptom; the cause is that the new VM has no story for cyclic
  reachability, and `Rc` is structurally incapable of providing one.
  Every `Rc<RefCell<T>>` in the value model becomes a `Gc<T>` handle.
- **Blocker (the symptom):** `crates-next/otter-vm` is built on
  `Rc<RefCell<…>>` for every heap-shared type. `WeakMap`/`WeakSet`
  keep strong refs by design (`collections.rs:6, 408`); heap caps in
  `Runtime::max_heap_bytes()` are documented as *informational only*
  (`crates-next/otter-runtime/src/lib.rs:670–675`). Result:
  long-running tests OOM the host.
- **Strategy: take the 2026 state-of-the-art directly.** The legacy
  `crates/otter-gc/` already implements a V8/JSC-shaped page-based
  generational heap (~5700 LOC, semispace scavenger + tri-color
  marking + write barriers + atomic header + handle scopes,
  unit-tested in isolation). It was modelled after V8 Orinoco / JSC
  Riptide as of 2026 and is exactly the code we would re-derive if
  we re-walked the V8 1997→2026 evolution. **We do not re-walk it.**
  Phase 1 ports the page-based heap into `crates-next/otter-gc`
  *directly*, with write barriers wired from day 1 — not a
  handle-table interim. There is no "Phase 1 worse than legacy"
  step.
- **One migration sweep, not two.** `Gc<T>` is a real
  `NonNull<GcHeader>` from the start. Inserting write barriers at
  every pointer store in `otter-vm` is unavoidable in any
  production-grade design — we pay that audit cost once, in Phase 1,
  not twice (slot-table-now → page-heap-later).
- **ADR amendment:** ADR-0001 §5 forbids `unsafe_code` in
  `crates-next/*`. The page-based heap is unsafe-heavy (~40 unsafe
  blocks in legacy, all `// SAFETY:`-annotated). Task 71 lifts the
  ban for `crates-next/otter-gc` only via ADR-0004; the rest of
  `crates-next/*` keeps `forbid(unsafe_code)`.
- **What we do not import from V8/JSC in Phase 1:** incremental
  marking (Phase 2), concurrent marking (Phase 3, deferred
  indefinitely), Mark-Compact (Phase 3). Base STW old-gen mark-sweep
  + young-gen scavenge is enough to clear the test-sweep blocker; ≤ 5
  ms pause target moves to Phase 2.
- **What we DO import in Phase 1, beyond the legacy crate's surface
  (because retrofitting them later is multi-month):**
  - **Pointer compression in `Gc<T>` / heap-side `Value`** — V8-style
    32-bit compressed pointers within a 4 GiB cage. Halves heap
    footprint on object-heavy workloads vs. 64-bit pointers. This is
    a decision-now-or-never call: it shapes `Value`, `Gc<T>`,
    `GcHeader`, and every barrier signature. We commit now.
  - **Card-table remembered set** (bitmap per-page, ~512 B cards),
    not the flat `Vec` the legacy crate ships with. Same barrier
    public API; better cache behaviour and zero-allocation barrier
    fast path.
  - **Black allocation during marking** — newly-allocated objects
    start black if `is_marking == true`, so the marker doesn't
    re-trace allocations born during the cycle. ~20 LOC; standard in
    every modern GC.
  - **`.heapsnapshot` DevTools-format writer** — promoted from
    optional (§7.1) to a Phase-1 deliverable. Production-debugging
    parity with V8 Inspector / Chrome DevTools costs nothing extra
    given we already walk the heap for `HeapSnapshot`.

---

## 1. Requirements

### 1.1 Functional

| F# | Requirement | Spec / source |
|----|-------------|---------------|
| F1 | Reclaim cyclic object graphs (`a.b = c; c.d = a`). | ECMA-262 §6.1.7 implicit |
| F2 | Honour `WeakMap`/`WeakSet`/`WeakRef` semantics: entries become unreachable when keys die. | ECMA-262 §24.3, §24.4 |
| F3 | Run `FinalizationRegistry` callbacks once a held value's target is collected. | ECMA-262 §26.2 |
| F4 | Surface a *catchable* `RangeError` (`OtterError::OutOfMemory`) when the configured heap cap is exceeded — never a process abort. | matches legacy `gc.rs:120` `oom_flag` |
| F5 | Trace every reachable strong reference: globals, current call frames (locals + register window + accumulator + `this` slot), microtask queue, promise reaction queues, parked async/generator frames, module environments, deferred dynamic-import host. | inferred from `lib.rs` survey (§4) |
| F6 | Be re-entrant safe: GC must not run during native re-entry that holds a `RefMut<…>` borrow on a heap object. (Today, `RefCell`-on-everything makes any safepoint dangerous; the new model removes interior mutability from the public handle path.) | self |
| F7 | Support `>1` runtime instance per process (one isolate = one heap, no shared state). | matches legacy MEMORY.md |

### 1.2 Non-functional (production-grade bar)

> **Time is not the constraint; production-grade is.** Phase 1 does
> not close on "tests pass". It closes on V8/JSC-comparable
> measurements across throughput, pause time, peak RSS, allocation
> rate, miri cleanliness, and stress-test endurance.

| NF# | Budget | Rationale |
|-----|--------|-----------|
| NF1 | **Phase-1 STW old-gen pause:** ≤ 50 ms at 256 MB live, ≤ 200 ms at 1 GB live. **Phase-1 young-gen scavenge:** ≤ 5 ms at 4 MB nursery (warm), ≤ 10 ms at 16 MB nursery. **Phase-2 production target:** ≤ 5 ms steady-state at 1 GB. | V8 Orinoco published budgets; not a "test workload" budget |
| NF2 | **Allocation throughput:** ≤ 10 ns per young-gen `alloc()` in the bump fast path. ≤ 30 ns including barrier-needing pointer field initialisation. | bump alloc + `NonNull<GcHeader>` write; matches V8 inline-cache path within 2× |
| NF3 | **Peak RSS:** every test sees the configured `max_heap_bytes_per_test` cap (default 512 MB) **honoured**, OOM surfaces as `OtterError::OutOfMemory`. Parent process never exceeds 4 GB RSS during full test262 sweep. **Steady-state RSS bloat (24 h embedder):** ≤ 1.5× live-set size. | unblocks tests; production embedder bar |
| NF4 | **Determinism:** Reachability is deterministic — a value reachable through any documented strong root survives at least one collection. Finalisers run on a deterministic phase boundary (post-sweep, before next mutator step). Cycle timing is best-effort. | matches V8/JSC |
| NF5 | **Single-mutator isolate.** One isolate owns one mutator at a time. `RuntimeCore`, `RuntimeState`, `GcHeap`, `Gc<T>`, `Local<'gc, T>`, and native contexts are `!Send + !Sync`. Public `Otter` / `RuntimeHandle` may be `Send + Sync` only because they send commands to an isolate runner and never expose VM / GC references. The cap-enforcement flag, command queues, event-loop wakeups, and (Phase 2) sweeper hand-off queue are the only `Sync` surfaces. | matches MEMORY.md + ADR-0005 |
| NF6 | **Diagnostic surface:** `gc.heap_snapshot() -> HeapSnapshot`; `gc.write_devtools_snapshot(path)` (Chrome DevTools format); per-type retained-byte counters; deterministic `gc.collect_full()` for repro tests; `gc.stats()` exposes pause-time histogram + allocation rate + GC cycle count. | required for production debugging |
| NF7 | **Memory safety bar.** Every `unsafe` block annotated with `// SAFETY:`; every public `unsafe fn` documents `# Safety`; every non-trivial unsafe block has a miri test; **`cargo +nightly miri test -p otter-gc` green** on every PR that touches `crates-next/otter-gc/`. **`cargo asan test`** (AddressSanitizer) green nightly. | production-grade hygiene; matches V8 / JSC fuzz-testing bar |
| NF8 | **Stress-test endurance.** Phase-1 closeout (task 84) includes: (a) 24 h continuous test262 loop without OOM / crash / RSS drift; (b) `loom` model-checking pass on the (Phase 2) concurrent sweep hand-off; (c) `cargo fuzz` corpus on the heap API surviving 10 M iterations no-panic. | production-grade endurance; not negotiable |
| NF9 | **Pointer-compression invariants.** All heap pointers fit in 4 GiB cage; cage exhaustion surfaces `OutOfMemory` cleanly; no UB on cage-base recomputation across `Runtime` reuse. miri-tested. | matches V8 sandbox |
| NF10 | **Throughput parity bar (Phase 2 exit).** On a curated benchmark suite (allocation-heavy: object literals, closures, JSON parse; pause-sensitive: long-running async/await chains), end-to-end throughput within **30 % of V8 (Node.js current LTS)** at the same pause-time SLO. Sub-30 % is acceptable; sub-50 % triggers a perf-track review before Phase 3 starts. | objective production-grade benchmark, not vibes |

---

## 2. Legacy GC analysis (`crates/otter-gc/`) — design reference only

> **Working rule** ([`README.md`](./tasks/README.md) §Working rules
> 1–2): every line under `crates-next/*` is **new code**. We do not
> import, link, or paste from `crates/*`. The legacy GC crate is
> *design reference* — we read it to understand algorithms and
> invariants, then rewrite under the new module-docstring +
> spec-link conventions inside `crates-next/otter-gc/`.

The legacy crate already encodes our domain constraints. Two
coexisting layers:

### 2.1 The safe layer — `typed.rs` (770 LOC, **wired**)

```text
TypedHeap
 ├── slots: Vec<Option<Slot>>            // handle table; Handle = u32 index
 ├── free_list: Vec<u32>                 // recycled slot indices
 ├── tracked_bytes: usize                // running heap-byte total
 ├── max_heap_bytes: Option<usize>       // hard cap (informational + enforced)
 ├── oom_flag: Arc<AtomicBool>           // shared signal, polled at safepoints
 ├── mark_phase / mark_additional / sweep_phase
 └── reserve_bytes / release_bytes       // off-slot accounting (Vec growth, …)
```

- **Algorithm:** STW mark-sweep over a handle-table, BFS from roots,
  `Vec<bool>` mark bitmap parallel to `slots`. Sweep frees `Box<dyn
  TypeErasedObject>` slots and pushes indices to `free_list`.
- **Tracing:** a `Traceable` trait — every type implements
  `fn trace_handles(&self, visitor: &mut dyn FnMut(Handle))`. Type
  erasure via `Box<dyn TypeErasedObject>` in slot storage. One vtable
  hop on the trace path. Acceptable.
- **Ephemerons:** `run_mark_phase` + `run_mark_additional` + `run_sweep_phase`
  are exposed as separate entry points so callers can run a fixpoint
  for `WeakMap` values between mark and sweep (legacy MEMORY.md
  §"Ephemerons for WeakMap/WeakSet (2026-02-08)"). This is a *known
  good* pattern — port it verbatim.
- **Heap cap:** `would_exceed_limit` + `oom_flag` enforced in
  `alloc::<T>()` and `reserve_bytes()`. **Caveat (PRODUCTION_READINESS
  §2.2):** in the legacy code `would_exceed_limit` *sets* the OOM flag
  but the alloc still runs. We will tighten this in the port:
  `would_exceed_limit → return Err(OutOfMemory)` *before* the slot is
  allocated.

**This subsystem is portable as-is.** Only changes needed:

1. Convert `Handle(u32)` to a typed `Gc<T>` newtype wrapping `(u32, PhantomData<T>)`
   — the legacy crate untyped `Handle` and re-derived type via
   `as_any().downcast_ref::<T>()` on every read. We pay one `Box<dyn>`
   indirection plus a downcast on the hot read path. **Replace with a
   discriminant `u8 type_tag` stored in the slot** — same trick the
   legacy `header.rs` uses; lookup via `&'static fn(&dyn Any)` table is
   O(1).
2. Move `tracked_bytes` enforcement to *pre-allocate*.
3. Drop `slots_ptr()` (JIT escape hatch — irrelevant in Phase 1).

### 2.2 The unsafe layer — `heap.rs`/`page.rs`/`scavenger.rs`/etc. (**unwired**)

```text
GcHeap (page-based)
 ├── new_space: NewSpace      // semispace, bump alloc, Cheney scavenger
 ├── old_space: OldSpace      // mark-sweep with free lists
 ├── large_space              // one page per object > PAGE/2
 ├── trace_table: TraceTable  // type_tag -> fn(*const GcHeader, &mut visitor)
 ├── handle_stack: HandleStack          // RAII Local<T>/HandleScope
 ├── global_handles                     // explicit-drop globals
 ├── write_barrier: WriteBarrier        // remembered set + Dijkstra insertion
 └── marking: MarkingState              // tri-color worklist; incremental-able
```

- **What works:** the design is V8/JSC-shaped (256 KB pages, 8-byte
  `GcHeader` with atomic flags, payload area starts after page header).
  All structures are unit-tested. Forwarding pointers, mark-bit reset,
  incremental drain budget — all present.
- **What was never wired:** no call site outside `otter-gc` itself
  calls `alloc_typed`, `alloc_old`, `WriteBarrier::record`, or any of
  the page-walk APIs. The legacy `otter-vm` keeps using
  `TypedHeap::alloc<T>()`. The `gc-migration-plan.md` is the
  multi-month roadmap to migrate, currently at "Phase 2.7" of step 2
  of *9 phases per type variant*. This is precisely the pit we are
  not willing to fall into a second time.
- **Known issues** (from `PRODUCTION_READINESS_PLAN.md` §2.2 and the
  migration plan):
  - `Vec`-backed container growth (object property storage, array
    elements, `BigInt` digits) is only accounted when the caller
    manually invokes `reserve_bytes()`. **Forgotten reservations
    silently bypass the cap.** No test enforces the protocol.
  - The `TypedHeap::alloc` cap check sets `oom_flag` but does **not**
    abort the alloc — see §2.1 (1).
  - `is_marking` write-barrier flag never observed by a real
    incremental marker — barrier code exists but the trigger does not.
  - `slots_ptr()` is a raw-pointer escape hatch for the legacy JIT;
    irrelevant for the new engine.
- **Why it was not ported as-is:** ADR-0001 forbids `unsafe_code` in
  `crates-next/*`. The page-based layer has ~40 unsafe blocks
  (`PRODUCTION_READINESS_PLAN.md` §2.5). The new-engine team
  deliberately deferred GC until ECMA-262 spec coverage stabilised —
  task pool README says *"GC and JIT are explicitly out of scope … each
  gets their own architectural plan once spec coverage is complete"*.
  That moment is now.

### 2.3 Inheritance ledger (revised — production-grade from Phase 1)

| Component | Decision | Justification |
|-----------|----------|---------------|
| `Page` (256 KiB aligned, page-base bitmask) | **Reproduce in Phase 1** | Foundation of bump alloc + O(1) page lookup. No runtime cost reason to defer. |
| `GcHeader` (8-byte atomic: type tag, mark color, young flag, forwarding bit) | **Reproduce in Phase 1** | Pre-condition for both scavenger forwarding and tri-color marking. |
| `NewSpace` semispace + Cheney scavenger | **Reproduce in Phase 1** | Generational hypothesis covers most JS allocations; without this we eat full STW pauses on every GC. |
| `OldSpace` mark-sweep + free-list | **Reproduce in Phase 1** | Old-gen reclamation; survivors land here on second scavenge. |
| `LargeObjectSpace` | **Reproduce in Phase 1** | Half-page+ allocations get dedicated pages; avoids fragmentation. |
| Tri-color `MarkingState` worklist with `drain_with_budget` | **Reproduce in Phase 1**, drive STW in Phase 1 + budget-driven incremental in Phase 2 | Same code path serves both modes; no rewrite needed when Phase 2 lands. |
| `WriteBarrier` (generational + Dijkstra insertion) + `RememberedSet` | **Reproduce in Phase 1** | Required by Cheney scavenger (old→young pointer detection). Insertion barrier is a no-op until incremental marking turns on; cost ~2 ns. |
| `TraceTable` (function-pointer table indexed by `type_tag`) | **Reproduce in Phase 1** | One indexed load + indirect call; matches V8 `BodyDescriptor::IterateBody`. **No `Box<dyn>`, no downcast, no vtable.** |
| `HandleStack` / `LocalHandle` / `GlobalHandle` (V8-style RAII rooting) | **Reproduce in Phase 1** | Required because Phase 1 is moving (scavenger relocates young objects). Local handles let native code hold pointers across safepoints. |
| Ephemeron fixpoint (`run_mark_phase` / `mark_additional` / `sweep_phase` split) | **Reproduce in Phase 1** | Correctness for `WeakMap`/`WeakSet`/`WeakRef`/`FinalizationRegistry`. |
| `oom_flag` + `OutOfMemory` + per-test cap enforcement | **Reproduce in Phase 1 with the legacy bug fixed** | Original sets the flag *and lets the alloc proceed*; revised refuses the alloc. |
| `TypedHeap` (handle-table, `Box<dyn TypeErasedObject>`) | **Discard** | This is the *interim* the legacy stack used while migrating away from `HeapValue`. We never adopt it — `Gc<T>` is `NonNull<GcHeader>` from day 1. |
| `slots_ptr()` JIT escape hatch | **Drop entirely** | New engine has no JIT yet; if it returns it goes through the public API. |
| **Pointer compression** (V8 sandbox; 32-bit compressed pointers in 4 GiB heap cage) — *new, beyond legacy* | **Add in Phase 1** | Decision-now-or-never. Halves heap footprint on object-heavy workloads vs. 64-bit pointers; matches V8 since 2020. Retrofitting later means rewriting `Value`, `Gc<T>`, `GcHeader`, every barrier. |
| **Card-table remembered set** (bitmap per page; ~512 B cards) — *new, beyond legacy* | **Add in Phase 1** | Replaces the legacy flat-`Vec` `RememberedSet`. Same barrier API; zero-alloc barrier fast path; matches V8/JSC. |
| **Black allocation during marking** — *new, beyond legacy* | **Add in Phase 1** | Allocations born during a marking cycle start black, so the marker doesn't re-trace them. ~20 LOC; standard in every modern GC. |
| **`.heapsnapshot` DevTools writer** — *new, beyond legacy* | **Add in Phase 1** | DevTools-format heap snapshot for production debugging. Reuses the `HeapSnapshot` walker (§7.1). |

### 2.4 Take-away

> **We are not building toward V8/JSC over five years.** The legacy
> `crates/otter-gc/` is V8/JSC-shaped 2026 code that simply was never
> wired into the legacy VM. Phase 1 ports its design (under new
> conventions: ADR-0001 §6 module docstrings, mandatory ECMA-262 spec
> links, `thiserror` errors, no `Box<dyn>` on the trace path) into
> `crates-next/otter-gc/` and wires it through the per-type
> migrations (tasks 76–83). We pay the write-barrier audit cost
> exactly once.

---

## 3. V8 / JSC literature scan

Each technique below is annotated with applicability *to our workload*.
The workload signature: small-to-medium heaps (≤ 1 GB), low-millisecond
allocation rate during test262 tight loops, single-threaded mutator,
non-realtime pause tolerance for the test sweep but ≤ 10 ms is the
production goal.

| Technique | Applicable? | Rationale |
|-----------|-------------|-----------|
| **V8 generational hypothesis (young/old)** | ✅ Phase 3 | JS allocation profile is canonical: most objects die young (Closures, iteration tuples, intermediate strings). A nursery cuts STW cost by 5–10× without the complexity of full incremental. |
| **V8 incremental marking + write barriers** | ✅ Phase 4 | Required to hit the ≤ 5 ms pause target. Cost: every heap-pointer store goes through a barrier. We accept the perf hit at hot paths because the alternative (long pauses on big heaps) blocks production embedding. |
| **V8 concurrent marking** | ❌ deferred indefinitely | Multi-threaded marking implies tri-color CAS plus a parking-mutator protocol. Not justified at our heap sizes. |
| **V8 pointer compression** | ❌ skip | Already covered by NaN-boxing. |
| **JSC Riptide constraint-based marking** | ⚠️ inspiration only | Useful conceptually (declarative root specification) but the impl complexity isn't justified — our root set is enumerable in <100 LOC (§4). |
| **JSC parallel marking** | ❌ same as concurrent | Out of scope. |
| **JSC no-read-barrier design** | ✅ adopt | We will *not* introduce read barriers. Read barriers cost too much on a mutator the size of ours; rooting via the call-frame walk is sufficient for an STW collector and survives into incremental (Dijkstra insertion barrier on writes only). |
| **Oilpan precise rooting via stack walk** | ⚠️ inspiration only | Cleaner than V8's HandleScope but assumes Blink's specific stack layout. The new VM's interpreter has structured frames (`Frame` in `lib.rs:4472`); we can enumerate roots from frames directly without a generic stack walk. |

**Summary:** V8 Orinoco is the closest blueprint. JSC's contribution
is mostly negative-space — *not* doing things — and that aligns with
keeping Phase 1 small.

---

## 4. Object graph and roots

### 4.1 Heap-shared types in `crates-next/otter-vm`

The types whose strong references must be reclaimable:

```
Value::Object(JsObject)                         // Rc<RefCell<ObjectBody>> today
Value::Array(JsArray)                            // Rc<RefCell<ArrayBody>>
Value::Map(JsMap), ::Set(JsSet)                  // Rc<RefCell<…Body>>
Value::WeakMap(JsWeakMap), ::WeakSet(JsWeakSet)  // ephemeron-table-backed
Value::Promise(JsPromiseHandle::Pure(…))         // Rc<RefCell<PurePromiseBody>>
Value::Iterator(Rc<RefCell<IteratorState>>)      // 7 places, lib.rs §iterator variants
Value::BoundFunction(Rc<BoundFunction>)
Value::NativeFunction(Rc<NativeFunction>)
Value::RegExp(JsRegExp)                          // wraps Rc<…>
UpvalueCell                                      // Rc<RefCell<Value>>; closures
JsString (when interned/cons-rope)               // already Arc-shared
```

After Phase 1 these become `Gc<T>` opaque handles. `Value` stays an
`enum` and stays small (`#[derive(Clone)]` cost goes from atomic
ref-count to a `u32` copy).

### 4.2 Root sources (the trace begins here)

Enumerated by reading `crates-next/otter-vm/src/lib.rs`:

| Root | Location | Notes |
|------|----------|-------|
| Globals | `RuntimeState::globals` (`JsObject`) | One handle. |
| Module environments | `RuntimeState::module_environments` map values (`JsObject`) | Strong refs to live module bindings. |
| Module URL keys | `Rc<str>` keys — strings, not GC. | No-op for tracing. |
| Intrinsics table | `RuntimeState::intrinsics` (well-known prototypes/constructors) | Always reachable; treat as global. |
| Microtask queue | `MicrotaskQueue` entries (closures, promise reactions) | Each microtask carries handles to settled values + handler closures. |
| Promise reaction graph | reachable transitively from Promises in the microtask queue / parked frames | Phase 1 leaves Promise traversal to `JsPromise::trace_handles`; no special-case. |
| Call stack frames | `Interpreter::frames: Vec<Frame>` | Each frame's locals + register window + `this` slot + accumulator + bytecode-module reference. |
| Parked async/generator frames | `Rc<Cell<Option<Box<Frame>>>>` slots in promise reactions (`lib.rs:4417, 4452`) | Treated as roots while parked. |
| Dynamic-import host | `module_loader.rs DYNAMIC_IMPORT_HOST` (thread-local) | Guarded; only set during `import()` — trace if `Some`. |
| Symbol registry | `SymbolRegistry` keyed by description | Symbols are GC-managed; entries are roots while the registry exists. |
| Active try/catch chain | error values pinned to landing pads | Implicit via frame walk. |

### 4.3 Pseudocode — root enumeration

```rust
// in crates-next/otter-vm/src/runtime_state.rs (new file)
impl RuntimeState {
    pub fn trace_roots(&self, v: &mut dyn FnMut(GcRaw)) {
        v(self.globals.gc_raw());
        for env in self.module_environments.values() { v(env.gc_raw()); }
        self.intrinsics.trace(v);
        self.microtasks.trace(v);
        for frame in &self.interpreter.frames { frame.trace(v); }
        if let Some(host) = DYNAMIC_IMPORT_HOST.with(|h| h.borrow().clone()) {
            host.trace(v);
        }
        self.symbol_registry.trace(v);
    }
}
```

`GcRaw` is `(u32, type_tag: u8)` — the slot index plus the discriminant.
The Phase 1 collector does not move objects, so a handle is a
permanent-address u32; no slot-rewriting needed. (Phase 3+ moving GC
will need a `*mut Gc<…>` slot-pointer pass.)

### 4.4 Handle survival across moves

Phase 1 is **moving** (Cheney scavenger relocates young objects on
every minor GC). `Gc<T>` is `NonNull<GcHeader>`. Two consequences:

1. **Pointer-stored roots survive moves** because the scavenger
   updates `*mut *const GcHeader` slots in-place — every root slot
   the GC walks is a *pointer-to-pointer*, not a value snapshot.
   `RuntimeState::trace_roots(v: &mut dyn FnMut(*mut *const GcHeader))`
   yields the address of each root pointer.
2. **Native code that holds a `Gc<T>` across a safepoint must root
   it** — `HandleScope` + `Local<'gc, T>`, V8/Oilpan pattern. The
   compiler statically prevents `Gc<T>` from outliving its
   `HandleScope`. Inside an interpreter step a raw `Gc<T>` is fine
   (no GC runs mid-step); native intrinsics that re-enter the
   interpreter must wrap their handles in `Local`.

`Gc<T>` is still `Copy` and `Send`-free. `Local<'gc, T>` is a
zero-cost wrapper that proves rooting at compile time.

---

## 5. Write / read barriers

| Phase | Barriers | Why |
|-------|----------|-----|
| Phase 1 (page-based generational STW) | **Generational write barrier** + **Dijkstra insertion barrier** wired from day 1; the latter is a no-op (`is_marking` always false in Phase 1) but the *insertion site* exists. | Generational barrier is mandatory once we have a young-gen scavenger; old→young pointers must be tracked in the remembered set or scavenge misses them. The insertion barrier lives at the same call site so Phase 2 lights it up by flipping a flag — no second audit sweep. |
| Phase 2 (incremental marking) | **Same insertion sites; `is_marking` flips on**. | Maintains tri-color invariant across marker/mutator interleaving. |
| Phase 3 (deferred — concurrent marking, compaction) | **Same barriers; CAS-shaded**. | Out of scope. |
| Any phase | **No read barriers.** | Mutator-side cost is unacceptable on the dispatch loop; tri-color marking requires no read barriers. |

**Barrier hot-path cost (Phase 3+):**

```
write_barrier(slot: Gc<T>, value: Gc<U>) {
    if marking_active && !value.is_marked() {
        marking_worklist.push(value);                  // ~5 ns
    }
    if slot.is_old() && value.is_young() {
        remembered_set.record(slot.as_raw_slot());     // ~5 ns
    }
}
```

A dispatch-loop pointer store is ~2 ns today; the barrier adds ~10 ns
worst case (one branch each, one push amortised). Acceptable on Phase
3+; we explicitly defer this cost for Phase 1.

**Barrier insertion sites** (every pointer store the VM performs):

- `JsObject::set_property` (every shape transition)
- `JsArray::set_index`, `JsArray::push`
- `JsMap::set`, `JsSet::add`
- `UpvalueCell::set`
- closure capture path (`MakeClosure`)
- `Promise.resolve` storing fulfilment value
- microtask enqueue when the closure carries a heap value

Each will be expressed as `gc.write_barrier(parent, child)` so a
single audit point owns the policy.

---

## 6. Integration with the Rust ownership model

### 6.1 Unsafe boundary

- **Phase 1**: `unsafe` is permitted in `crates-next/otter-gc/` only
  (ADR-0004 amendment to ADR-0001 §5; lands in task 71). Confined to
  `page.rs`, `space.rs`, `scavenger.rs`, `header.rs`, `handle.rs`,
  `barrier.rs`, `trace.rs`. The rest of `crates-next/*`
  (`otter-vm`, `otter-runtime`, `otter-cli`, …) keeps
  `#![forbid(unsafe_code)]`.
- **Hygiene** (mandatory, enforced in PR review):
  - Every `unsafe` block carries a `// SAFETY:` comment.
  - Every public `unsafe fn` documents preconditions in its
    docstring under `# Safety`.
  - Every non-trivial unsafe block has a corresponding miri test
    (`cargo +nightly miri test -p otter-gc`).
  - Spec links per [`README.md`](./tasks/README.md) §Working rules
    6 — every spec algorithm cites tc39.es/ecma262.

### 6.2 `Drop`, `Send`, `Sync`

- `Gc<T>: Copy` — no `Drop`. Cheap clone (u32+u8).
- `Gc<T>`, `Local<'gc, T>`, `HandleScope<'gc>`, `RuntimeState`,
  `NativeCtx<'_>`, and `GcHeap` are not `Send` or `Sync`. They never
  cross `.await`, `tokio::spawn`, worker boundaries, or public
  `RuntimeHandle` replies.
- The heap (`GcHeap`) owns all `Box<dyn>` payloads. Sweep drops them,
  invoking each payload type's `Drop` impl. This is how the legacy
  `WeakRef` / `FinalizationRegistry` callbacks fire — by registering
  a Drop-time hook on the slot.
- `GcHeap: !Sync, !Send`. One isolate has one mutator. In the product
  API this is enforced by running the heap inside an isolate runner and
  exposing only `Otter` / `RuntimeHandle` command surfaces to Tokio
  worker threads.
- Thread-local heap lookup (`GcHeap::with_thread_default*`) is not an
  architecture primitive. Product VM / runtime / native-binding code
  passes `RuntimeCx` / `NativeCtx` / `&mut GcHeap` explicitly so every
  allocation, dereference, and write barrier knows which isolate owns
  the handle. Any remaining thread-local helper is hidden test
  scaffolding only.
- Trace functions (`Traceable::trace_handles`) take `&self` and a
  visitor — never `&mut`. This prevents reentry-into-self during a
  trace.

### 6.3 Eliminating `RefCell` from the public path

The current `JsObject` is `Rc<RefCell<ObjectBody>>`. After Phase 1:

```
JsObject = Gc<ObjectBody>;            // no RefCell
impl ObjectBody {
    fn get(&self, key: &PropKey) -> ...;
    fn set(&mut self, key, val) -> ...;
}
GcHeap::get<T>(&self, gc: Gc<T>) -> &T;
GcHeap::get_mut<T>(&mut self, gc: Gc<T>) -> &mut T;   // exclusive heap borrow
```

The interpreter already threads `&mut RuntimeState` (task 56 —
`56-remove-refcell-from-hot-paths.md` — calls this out). Task 76A makes
that explicit through `RuntimeCx` / `NativeCtx`, and tasks 77-83 must use
that context rather than a thread-local heap. Borrow discipline is
`&mut cx.heap → &mut T` exclusively. Reentry (a native intrinsic that
re-enters the interpreter) needs to release the borrow before re-entry;
this is the same constraint the legacy crate
identifies as "ObjectCell zero-cost UnsafeCell pattern" (MEMORY.md §
"Key Decisions"). For the new engine, we surface this via
`heap.with_mut::<T, R>(gc, |t: &mut T| -> R)` to prevent the borrow
from outliving the closure scope.

### 6.4 GC safepoints

A *safepoint* is a point in execution where GC may run.

- Allocation slow path — when bump alloc in young-gen page fails, a
  scavenge runs.
- Bytecode dispatch back-edge (`cond_jump_backward`) — checks the
  cap-threshold flag and the (Phase 2) incremental-budget timer.
- Microtask drain boundary.
- Top of every native re-entry into the interpreter.

Phase 1 collection is STW. The borrow rule: a safepoint never runs
while the mutator holds a `&mut T` from `heap.get_mut(…)` — every
safepoint helper takes `&mut self`, which the borrow checker uses to
prove no other mutable borrow is live. Native intrinsics that hold a
`Gc<T>` across a safepoint must wrap it in `Local<'gc, T>` (§4.4).

---

## 7. Leak diagnosis

This is *part of the GC*, not a separate concern. The blocker we are
solving is unobservable without diagnostics.

### 7.1 Heap snapshot

```rust
struct HeapSnapshot {
    objects: Vec<Object>,           // type_tag, retained_size, owns: Vec<Gc<…>>
    roots: Vec<GcRaw>,
    edges: Vec<(Gc<…>, Gc<…>)>,
}
GcHeap::snapshot(&self) -> HeapSnapshot;
HeapSnapshot::write_chrome_heapsnapshot(&self, w: impl Write);   // optional
```

The legacy `otter-runtime` already implements a Chrome DevTools
snapshot writer (MEMORY.md §"P0/P1 Closeout"). Port it.

### 7.2 Per-type retained-size counters

```rust
GcStats {
    live_objects: usize,
    live_bytes: usize,
    by_type: [TypeStats; 256],     // bytes_live, alloc_count_total, free_count_total
    last_gc_pause_ms: f32,
    last_gc_reclaimed_bytes: usize,
}
GcHeap::stats(&self) -> &GcStats;
```

Exposed through `Runtime::heap_stats()` so tests can assert "this
script's `JsObject` count returns to baseline after `gc.collect_full()`".

### 7.3 Retained-size profiler

For the *blocker* — finding what holds memory in a leaky test — we
need a *retained-size walk*: BFS from roots, accumulating sizes per
type and per allocation site. Phase 1: implementable in ~200 LOC on
top of the snapshot. Phase 4 (production): emit a flamegraph-style
output via `otter-profiler`'s existing folded-stack format
(MEMORY.md §"P0/P1 Closeout: O3").

### 7.4 Repro harness

A test scaffold:

```rust
#[test]
fn weakmap_entry_collected_when_key_dies() {
    let mut rt = Runtime::builder().max_heap_bytes(64 * MB).build();
    rt.run_script("let m = new WeakMap(); { let k = {}; m.set(k, big_obj()); }").unwrap();
    rt.heap().collect_full();
    let stats = rt.heap_stats();
    assert!(stats.by_type[TYPE_OBJECT].live_bytes < 1024);
}
```

This single test, with its `WeakSet`/`FinalizationRegistry` siblings,
is the regression gate for the test262 OOM blocker.

### 7.5 Cap reproduction

Today `Runtime::max_heap_bytes` is informational. Phase 1 makes it
load-bearing — and immediately ungates the existing test262 runner's
`max_heap_bytes_per_test`, which is what the host machine actually
needs to survive the sweep.

---

## 8. Phased implementation plan

### Phase 1 — Page-based generational GC (`unsafe` permitted in `otter-gc`); unblocks the test sweep

**Scope:** stand up a V8/JSC-shaped generational GC in
`crates-next/otter-gc/` and migrate every `Rc<RefCell<…>>`-shared
`Value` variant onto it. Single migration sweep — no interim
backend.

- ADR-0004: lift `forbid(unsafe_code)` for `crates-next/otter-gc/`
  only; the rest of `crates-next/*` keeps the ban.
- New crate `crates-next/otter-gc/` reproduces (under new
  conventions) the legacy design surface:
  `header.rs`, `page.rs`, `space.rs`, `scavenger.rs`, `marking.rs`,
  `barrier.rs`, `trace.rs`, `handle.rs`, `heap.rs`, plus new
  `oom.rs`, `ephemeron.rs`, `finalize.rs`, `stats.rs`, `snapshot.rs`.
- Public API: `GcHeap`, `Gc<T>`, `Local<'gc, T>`, `HandleScope<'gc>`,
  `Traceable`, `TraceTable`, `OutOfMemory`, `GcStats`,
  `HeapSnapshot`.
- Cap enforcement (`Runtime::max_heap_bytes`) becomes load-bearing —
  refuses the alloc, surfaces `OtterError::OutOfMemory`.
- Per-type migration order (smallest blast radius first):
  1. `UpvalueCell` — cyclic by definition.
  2. `JsObject` — heart of the leak surface.
  3. `JsArray`, `JsMap`, `JsSet`.
  4. `JsWeakMap`, `JsWeakSet` (ephemeron fixpoint).
  5. `WeakRef`, `FinalizationRegistry` (new types).
  6. `JsPromiseHandle::Pure(…)`, `IteratorState`, generator state.
  7. `BoundFunction`, `NativeFunction`, `JsRegExp`.
- Write barriers wired at every pointer store in `otter-vm` (§5).
  Generational barrier load-bearing from day 1; insertion barrier
  is a no-op until Phase 2 flips `is_marking`.

**Exit criteria:**
- All `crates-next` lib tests still pass.
- `bash scripts/test262-safe.sh built-ins/Array` runs to completion
  on a 16 GB host without thrashing; peak host RSS ≤ 4 GB.
- Regression tests: cycle reclamation, WeakMap/WeakSet eviction,
  WeakRef clearing, FinalizationRegistry firing, cap-as-`RangeError`.
- STW old-gen pause ≤ 200 ms at 1 GB live (acceptable for tests;
  ≤ 5 ms target lands in Phase 2).
- Young-gen scavenge ≤ 5 ms on 4 MB nursery.
- Allocation ≤ 10 ns in fast path (bump in young page).

**ETA, single engineer, no surprises:** ~4–6 weeks. One unified
sweep — write barriers, handle scopes, and the page heap all land
together.

### Phase 2 — Incremental marking + concurrent sweeping + pretenuring

Single phase, three companion features that share the same
`drain_with_budget` infrastructure:

- **Incremental marking.** Flip `is_marking = true` at cycle start;
  drive `drain_with_budget` from the bytecode back-edge with a 1 ms
  step budget. Phase-1 insertion barriers go load-bearing — no new
  audit sweep across `otter-vm`.
- **Concurrent sweeping** of old-gen on a background thread.
  Foreground mutator parks on alloc only when it hits a partially-
  swept page. Matches V8 background sweep.
- **Incremental sweeping** of pages outside the concurrent budget,
  driven from the same back-edge tick.
- **Allocation-site pretenuring.** Each `Op::AllocObject` /
  `Op::AllocArray` carries a 16-bit profile counter; after N
  survivals from a site the runtime allocates that site directly to
  old-gen. V8/JSC standard.

**Exit criteria:** mutator sees ≤ 5 ms steady-state pause at 1 GB
live. Sweep no longer appears in mutator-thread pause-time
histogram. Pretenuring cuts young-gen scavenge pressure on
allocation-heavy benchmarks.

### Phase 3 — Mark-Compact, idle GC, sticky-mark-bit

- **Mark-Compact** for old-gen fragmentation. Sliding compactor
  with forwarding tables; runs only when fragmentation crosses
  threshold (V8 heuristic: > 30 % free-list slack).
- **Memory reducer / idle GC.** Proactive collection on idle
  callbacks — V8 standard. Prevents long-tail RSS growth in
  long-running embedders.
- **Sticky mark-bit minor cycles.** Old-gen mark bits persist
  across cycles; minor cycle only re-traces newly allocated /
  modified slots. V8 optimisation, big throughput win on
  steady-state workloads.

### Phase 4 (deferred indefinitely) — Concurrent marking + parallel scavenge

Single-mutator isolates make concurrent marking expensive in complexity
for marginal pause-time win. The public `RuntimeHandle` may be used from
many Tokio workers, but it still feeds one mutator per isolate. Defer
concurrent marking until production embedders demand it.

---

## 9. Files to create / modify in `crates-next/`

### New files (new crate `crates-next/otter-gc/`):

```
crates-next/otter-gc/Cargo.toml
crates-next/otter-gc/src/lib.rs           // public API + re-exports; module docstring per ADR-0001 §6
crates-next/otter-gc/src/header.rs        // 8-byte GcHeader: type_tag, mark color, young flag, forwarding
crates-next/otter-gc/src/page.rs          // 256 KiB aligned page; PageHeader; bump alloc; page-base bitmask
crates-next/otter-gc/src/space.rs         // NewSpace (semispace) / OldSpace (free-list) / LargeObjectSpace
crates-next/otter-gc/src/scavenger.rs     // Cheney young-gen copy + forwarding
crates-next/otter-gc/src/marking.rs       // Tri-color worklist; STW drain + drain_with_budget
crates-next/otter-gc/src/barrier.rs       // Generational + Dijkstra insertion; RememberedSet
crates-next/otter-gc/src/trace.rs         // TraceTable: type_tag → fn pointer, no Box<dyn>
crates-next/otter-gc/src/handle.rs        // Gc<T>, Local<'gc, T>, HandleScope<'gc>, GlobalHandle
crates-next/otter-gc/src/heap.rs          // GcHeap top-level: spaces + handle table + collection orchestration
crates-next/otter-gc/src/oom.rs           // OutOfMemory + cap enforcement (refuse-the-alloc)
crates-next/otter-gc/src/ephemeron.rs     // WeakMap/WeakSet fixpoint API
crates-next/otter-gc/src/finalize.rs     // WeakRef + FinalizationRegistry post-sweep dispatch
crates-next/otter-gc/src/stats.rs         // GcStats per-type counters
crates-next/otter-gc/src/snapshot.rs      // HeapSnapshot, retained-size walker
crates-next/otter-gc/tests/cycles.rs      // cycle reclamation
crates-next/otter-gc/tests/ephemeron.rs   // WeakMap/WeakSet eviction
crates-next/otter-gc/tests/scavenger.rs   // young-gen copy + forwarding
crates-next/otter-gc/tests/oom.rs         // cap enforcement
crates-next/otter-gc/tests/handle_scope.rs // RAII rooting across safepoints
```

The legacy `crates/otter-gc/` is **design reference only** — read for
algorithm shape, do not import or paste from. Each new file opens
with the ADR-0001 §6 docstring (`Summary / Contents / Invariants /
See also`) and cites the ECMA-262 sections it implements.

### Modified files in existing crates:

```
crates-next/otter-vm/Cargo.toml                  // add otter-gc dep
crates-next/otter-vm/src/lib.rs                  // Value variants drop Rc<RefCell<…>>; replace with Gc<…>; root-tracing
crates-next/otter-vm/src/object.rs               // JsObject = Gc<ObjectBody>; impl Traceable for ObjectBody
crates-next/otter-vm/src/array.rs                // ditto for ArrayBody
crates-next/otter-vm/src/collections.rs          // Map/Set/WeakMap/WeakSet bodies; ephemeron wiring
crates-next/otter-vm/src/promise.rs              // PurePromiseBody; reaction-graph trace
crates-next/otter-vm/src/generator.rs            // generator-frame trace
crates-next/otter-vm/src/microtask.rs            // microtask trace, root contribution
crates-next/otter-vm/src/runtime_state.rs (new)  // central root enumeration (currently inlined in lib.rs)

crates-next/otter-runtime/src/lib.rs             // wire max_heap_bytes from informational → enforced
crates-next/otter-runtime/src/error.rs           // OutOfMemory bridges otter_gc::OutOfMemory

crates-next/otter-test/...                       // optional: harness opt-in for heap snapshots in failing tests

docs/new-engine/adr/0004-gc-introduces-unsafe.md // (Phase 3 only) amend ADR-0001
```

### Files **not** touched by Phase 1:

- `otter-bytecode`, `otter-syntax`, `otter-compiler` — they don't see
  heap values.
- `otter-cli` — only the runtime API surface changes.
- `otter-test262` — already correctly wired through `Runtime::max_heap_bytes`.

### What we explicitly do not do

- **Do not** add a path-dep on `crates/otter-gc/` from any
  `crates-next/*` crate. ADR-0001 keeps `crates/*` excluded from the
  workspace; that exclusion stands. Read the legacy crate for design,
  rewrite under new conventions.
- **Do not** ship a "handle-table interim". `Gc<T>` is
  `NonNull<GcHeader>` from the first commit.
- **Do not** introduce `Box<dyn TypeErasedObject>` or `dyn Any`
  downcast on the trace path. The trace dispatch table is a flat
  `[Option<TraceFn>; 256]` indexed by `type_tag`.

---

## 10. Risks and open questions

### 10.1 Risks

| R# | Risk | Likelihood | Mitigation |
|----|------|------------|------------|
| R1 | Phase-1 port surfaces `RefCell` borrow-mid-trace bugs (a trace function transitively calls back into `JsObject::get`, which tries to borrow again). | medium | Make `Traceable::trace_handles(&self)` take `&self` only; trace functions read shape/keys but never re-enter the interpreter. Audit every impl in PR. |
| R2 | Microtask queue and parked-frame slots are missed during root enumeration → live values get reaped → spurious test failures. | medium | Pre-Phase 1: write a `gc_smoke_test` that allocates one of each value variant, drops the local binding, runs `gc.collect_full()`, and asserts the value still lives because it is in the microtask queue / module env / etc. One root → one regression test. |
| R3 | `WeakMap` ephemeron fixpoint loops infinitely on pathological self-referential graphs. | low | Legacy impl has a deterministic worklist-based fixpoint with no infinite-loop case (terminates when no new objects are marked in a pass). Port the test set verbatim. |
| R4 | Heap-cap enforcement exposes pre-existing leaks in non-`crates-next` callers (Intl, Temporal, RegExp via `regress`). | medium | These types use ICU / external crates with their own allocators. Their bytes are *not* on our heap and not subject to the cap. Document explicitly: cap covers GC-managed objects only; native allocations escape. Add a separate `process_rss_threshold` watchdog if needed. |
| R5 | ADR-0001 amendment for Phase 3 unsafe reintroduction is rejected. | low | Phase 1 doesn't need it; the project survives indefinitely on Phase 1. Phase 3 is a perf win, not a correctness gate. |
| R6 | Migrating `JsObject` mid-flight breaks 1000+ call sites in the new VM. | high | Land Phase 1 as a series of per-type commits (UpvalueCell → JsObject → …); each commit individually green. Don't bundle. |
| R7 | The handle-table heap's `Box<dyn>` layout makes hot-path object access slower than `Rc<RefCell<…>>` after the borrow check is amortised. | medium | Two indirections (`slots[idx] → Box → T`) vs. one (`Rc → T`). Measured legacy cost: ~5 ns extra per object access. Acceptable. If profile shows otherwise, Phase 2 can replace `Box<dyn>` with a direct typed slot via `type_tag` discriminant. |

### 10.2 Open questions

| Q# | Question | Owner / next step |
|----|----------|-------------------|
| Q1 | Does `Value::Function { function_id }` need to participate in tracing, or is the `BytecodeModule` always rooted via the active call frame? | Audit `Frame::module_ref` lifetime; expected answer: yes, frame holds the module strong, no GC handle needed. Confirm before Phase 1 starts. |
| Q2 | `JsString` is currently `Arc`-shared with rope storage. Do we put strings on the GC heap, or keep them in a separate string-intern arena? | Recommend **separate arena**. Strings have different lifetime characteristics (interning, immutability, no children). Legacy crate eventually moved them onto the GC heap (§"C2 String Hierarchy" in MEMORY.md) but that's a Phase 5+ optimisation. Phase 1: keep `Arc<JsString>` and treat string handles as leaves (no `trace_handles` work). |
| Q3 | Do we register `FinalizationRegistry` callbacks on every object alloc unconditionally, or only when at least one registry exists? | Lazy: a per-heap `bool has_registries` flag short-circuits the post-sweep walk. |
| Q4 | What's the policy for `RegExp` (`regress`)? `regress::Regex` allocates internally and isn't traceable. | Wrap in a leaf `Gc<…>` that owns the `regress::Regex` and is trivially traced; `regress`-internal allocations are out of cap. Document explicitly. |
| Q5 | The legacy `tracked_bytes` is approximate (`size_of::<T>()` only). Do we need exact accounting (`Vec` capacity etc.) for the cap to be useful? | Phase 1: approximate is enough — the cap fires before exact RSS does, so users see `RangeError` instead of OOM-kill. Phase 2 can add `reserve_bytes` for hot containers (`ArrayBody::elements`). |
| Q6 | Should the GC be exposed to JS as a debug-only `gc()` global? | Yes, gated behind the existing `--allow-gc` capability flag (which doesn't exist yet — file as a small follow-up). |

---

## 11. ADR amendment trigger

Phase 1 amends ADR-0001 §5 in **one** step: a new crate
`crates-next/otter-gc/` is added to the workspace **and** is
permitted `unsafe_code` (every other `crates-next/*` crate retains
the ban). The amendment lands in task 71 — same commit as the empty
crate skeleton, before any unsafe block ships.

Draft ADR-0004 outline:

```text
docs/new-engine/adr/0004-gc-crate-and-unsafe-boundary.md
- Amends: ADR-0001 §5
- Status: accepted
- Decision:
    1. Add `crates-next/otter-gc/` to the workspace `members`.
    2. Lift `#![forbid(unsafe_code)]` for this crate only.
    3. All other `crates-next/*` crates keep the ban.
- Constraints (mandatory, enforced in PR review):
    * Every unsafe block carries a `// SAFETY:` comment.
    * Every public `unsafe fn` documents preconditions in its
      docstring under `# Safety`.
    * Every non-trivial unsafe block has a corresponding miri test.
    * Module docstrings + ECMA-262 spec links per ADR-0001 §6 and
      [`tasks/README.md`](../tasks/README.md) §Working rules 6.
- Boundary: see §6.1 of `docs/new-engine/gc-architecture.md` for the
  unsafe surface inventory.
```

---

## 12. What this document is *not*

- A code patch. Implementation is the next track.
- A line-by-line port. Phase 1 takes the *design* of legacy `typed.rs`,
  not its source verbatim — naming conventions, error types, and the
  type-tag dispatch table are new-engine-shaped.
- A perf claim. The pause/throughput numbers in §1.2 and §8 are
  targets, not measurements.
- A schedule. ETAs are calibrations for sequencing decisions, not
  commitments.

The next concrete deliverable is a Phase-1 implementation skeleton:
`crates-next/otter-gc` empty crate + the `Traceable` trait + the
first migrated type (`UpvalueCell`) + one passing cycle-reclamation
test. Everything else flows from there.
