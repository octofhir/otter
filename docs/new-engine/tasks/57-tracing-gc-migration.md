# Task 57 — Write our own tracing GC and replace `Rc` everywhere

## Goal

Write a tracing garbage collector tailored to Otter's value model
and migrate every `Rc<T>` / `Rc<RefCell<T>>` in `crates-next/*`
to GC-managed handles. The end state matches what V8 / JSC /
SpiderMonkey do: every shared JS value is a GC-traced heap
allocation, with zero per-clone refcount work and no
deterministic-drop reference cycles.

**No third-party GC.** We do not adopt `boa_gc` — it is
designed around Boa's value model (which is not ours), is
maintained by an external project that has signaled an intent to
rewrite it, and adopting it would couple our perf ceiling to
their pace of work. Writing our own keeps the value model and
the collector co-designed, which is how every production engine
ships them.

## Why this is critical

Refcounting is **the wrong abstraction** for a production JS
engine, for three independent reasons:

1. **Per-clone overhead.** Every `Rc::clone()` is a counter
   increment + a counter decrement on the eventual drop. On the
   hot path (every register write, every value-passing call) we
   eat a load + add + store + branch per clone. A tracing GC
   collects en-masse and pays nothing per assignment.
2. **Cycles leak.** Object → method → captured-`this` → object
   round-trips do not free under refcounting. Real JS code
   produces these constantly. Without `Weak<T>` discipline (which
   we don't have anywhere) we silently leak. A tracing GC
   reclaims cycles by definition.
3. **Cache pressure.** Every `Rc<T>` is a 16-byte header (strong
   + weak counts) preceding the payload. JS-heavy workloads
   touch hundreds of allocations per millisecond; the header
   bloats cachelines and hurts hit rate. A tracing GC packs
   payloads tightly.

Reference: V8's `Local<T>` / `Handle<T>` (no refcounting),
JSC's `JSCell*` (no refcounting), Boa's `boa_gc::Gc<T>` (custom
generational GC). None of them use `Rc`.

## Scope

1. **Inventory.** List every `Rc<T>` and `Rc<RefCell<T>>` in
   `crates-next/*`. Rough catalog as of task 34:
   - `JsObject` — `Rc<RefCell<ObjectBody>>`
   - `JsArray` — `Rc<RefCell<ArrayBody>>`
   - `JsString` — `Arc<StringRepr>` (cross-thread; same migration)
   - `JsRegExp` — `Rc<JsRegExpBody>`
   - `BoundFunction` — `Rc<BoundFunction>`
   - `ClassConstructor` — `Rc<ClassConstructor>`
   - `IteratorState` — `Rc<RefCell<IteratorState>>`
   - `PurePromise` — `Rc<RefCell<PurePromiseBody>>`
   - `NativeFunction` — `Rc<NativeFunction>`
   - `Closure::upvalues` — `Rc<[UpvalueCell]>`
   - `UpvalueCell` — `Rc<RefCell<Value>>`
2. **Design our own GC.** Targets:
   - Single-threaded incremental mark-and-sweep for v1; lay
     ground for generational + concurrent later.
   - `Gc<T>` handle (one machine word, no header). `GcCell<T>`
     replaces `RefCell<T>` so the borrow flag goes too.
   - `derive(Trace)` proc-macro on every value-graph type
     (coordinated with task 55 — `otter-macros-next` generates
     the impls).
   - Allocator is bump-pointer in a young-generation arena;
     promotion to old generation on second survival. Old gen is
     mark-sweep-compact.
   - Roots are live `Frame::registers`, the microtask queue, the
     module's constant pool, and the embedder's external handle
     table.
   - No `Drop` semantics in GC types — `Finalize` runs at sweep
     time on collected objects only. `Cell<u32>`-style fields
     stay because they're plain bytes.
3. **Cap the GC interface to a small surface.** `GcHandle<T>` /
   `GcCell<T>` / a `Trace` derive. Everything below that is
   gc-implementation-private.
4. **Migrate one type at a time.** Order by hot-path impact:
   `JsString` (most-cloned) → `JsObject` → `Closure::upvalues` →
   `Promise` → the rest.
5. **Per-type bench.** Add a criterion bench before and after
   each migration. Report the delta. The point of this work is
   measurable perf, not architectural taste.

## Out of scope

- Migrating `crates/*` legacy stack.
- Removing GC for embedders that want a per-script lifetime.
- Concurrent / parallel GC. Single-threaded incremental is
  enough for foundation; concurrent comes after task 35 (async)
  introduces multi-threaded value sharing.

## Acceptance criteria

- `rg "\bRc<" crates-next/otter-vm/src/` returns zero hits in
  value types. `Rc` may still appear in non-value-graph code
  (e.g. configuration handles).
- A criterion bench suite reports at least a 1.3× speedup on
  the call-heavy, string-heavy, and promise-chain workloads.
- Engine fixture suite stays at 100% green throughout the
  migration.
- Cycles introduced by JS programs no longer leak (test: run a
  script that builds 100k cyclic objects, verify resident set
  stays bounded).

## Coordination

- **Task 56** (`RefCell` removal) is partially redundant with
  this — once a GC lands, `GcCell<T>` replaces `RefCell<T>` and
  the borrow-flag overhead disappears too. Coordinate so we don't
  do the same migration twice.
- **Task 35** (async/await) cannot ship `SharedArrayBuffer` /
  worker threads without a thread-aware GC. Sequence so async
  lands on top of the GC, not the other way around.
- **Task 55** (`otter-macros-next`) — the proc-macro should
  generate `derive(Trace)` for value structs once the GC is in.

## Risks

- Largest single change in `crates-next/*` so far. Plan in slices
  of one type per PR, not one mega-PR.
- `Trace` discipline matters: missing a field in a `Trace` impl
  is a use-after-free. The derive macro covers most cases; audit
  the rest.
- Some patterns rely on Rc's deterministic-drop semantics
  (e.g. `JsRegExp::lastIndex` interior `Cell`). GC migration
  must preserve those semantics through `Drop` impls or
  explicit `Finalize` hooks.

## Status

- **deferred until full ES spec coverage is in.** Foundation
  goal first: cover the JS spec end-to-end on the simple `Rc`
  model. GC and (later) JIT each ship as their own dedicated
  plan + crate after the spec coverage is solid. This task is
  the placeholder for the GC plan; do not start it before the
  spec-coverage gate.
