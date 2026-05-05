# Task 94 — Contributor-facing GC / VM API surface

## Status

- [ ] public/internal GC API boundary documented
- [ ] safe extension/native allocation API designed around
      `NativeCtx` / `RuntimeCx` / task 93 branded sessions
- [ ] safe rooted handle tiers designed: `Local`, `EscapableLocal`,
      `Root`, `Weak`
- [ ] safe mutation API guarantees write barriers without contributor
      call-site audits
- [ ] derive/macro path for trace implementations specified
- [ ] external/backing-store accounting API specified
- [ ] API docs include examples for builtin authors and embedders
- [ ] `docs/book` pages for GC API and contributor workflow updated
- [ ] compile-fail and doc tests cover misuse patterns
- [ ] gates green

## Goal

Make the GC/VM boundary clean enough that a new contributor can add a
builtin, host object, or extension module without learning the unsafe
internals of the collector and without accidentally bypassing rooting,
write barriers, weak semantics, or heap accounting.

The target balance is deliberate:

- **Safe by default:** the normal path for engine and extension work is
  safe Rust with compiler-enforced lifetimes, rooting, barriers, and
  isolate ownership.
- **Public enough to extend:** the API must be documented and ergonomic
  enough for contributors to add builtins, hosted modules, and host
  objects without copy-pasting VM internals.
- **Unsafe where it belongs:** low-level collector operations remain
  available to the collector and narrow VM adapter layers, but not as
  the default public model.
- **Escape hatches are explicit:** if a production embedding use case
  needs raw access, expose the smallest possible unsafe API with a
  `# Safety` contract, compile-fail coverage for misuse, and at least
  one positive integration test.

The collector backend remains page-based, moving, generational, and
unsafe-internal. The contributor-facing surface should feel closer to:

- V8's handle discipline: isolate-bound API, stack-scoped locals,
  persistent roots, weak persistent handles, and explicit external
  memory accounting.
- Boa's Rust ergonomics: `Gc<T>`-style typed handles and deriveable
  `Trace` / `Finalize` equivalents.
- Otter's stricter safety bar: task 93 branded sessions, no
  context-free weak upgrade, no thread-local heap lookup, and no
  contributor-written unsafe code for normal builtins.

Breaking interim APIs is allowed. A production-ready engine API is more
important than preserving early migration shapes.

## Source

- [`70-gc-master-tracker.md`](./70-gc-master-tracker.md) working rules.
- [`93-gc-branded-session-api.md`](./93-gc-branded-session-api.md).
- [`91-gc-bench-and-soak-infra.md`](./91-gc-bench-and-soak-infra.md)
  external/backing-store accounting requirements.
- [`95-contributor-book-and-extension-guides.md`](./95-contributor-book-and-extension-guides.md).
- V8 embedder API: `Isolate`, `HandleScope`, `Local`, `Global` /
  `PersistentBase`, weak persistent handles, and external memory
  adjustment.
- Boa `boa_gc`: `Gc`, `WeakGc`, `GcRefCell`, `Trace`, `Finalize`, and
  derive macro ergonomics.

## Comparison Notes

### V8 API lessons

V8's public embedding API does not expose raw heap pointers as the main
extension surface. It forces embedders through an isolate and handle
model:

- `Local<T>` is short-lived and belongs to a `HandleScope`.
- `EscapableHandleScope` is required to return a local out of a nested
  scope.
- `Global<T>` / persistent handles survive beyond a stack scope and are
  explicitly reset/disposed.
- persistent handles can become weak and receive GC callbacks.
- external memory is reported to the isolate so GC heuristics see
  native/backing-store pressure.

Otter should copy the API shape, not the C++ ownership model: stack
scope, persistent root, weak root, isolate parameter, and external
memory accounting are the important concepts.

### Boa API lessons

Boa's `boa_gc` is approachable for Rust contributors because normal code
uses typed `Gc<T>` pointers, `WeakGc<T>`, `GcRefCell<T>`, and deriveable
`Trace` / `Finalize` traits. That lowers the barrier to adding engine
objects.

Otter should copy the ergonomics, but not the weaker safety boundary:

- avoid context-free weak upgrade;
- avoid dynamic borrow-heavy `GcRefCell` as the default object model;
- keep finalization rules explicit and isolate-local;
- keep moving-GC barriers hidden behind safe mutation APIs.

### Current Otter gap

`crates-next/otter-gc` currently exposes the right primitives for VM
migration, but too many of them are backend-shaped:

- `RawGc` and raw slot visitors are necessary internally, but should not
  be the normal extension API.
- contributors can still reach APIs that imply manual barrier/rooting
  discipline.
- `SafeTraceable` exists, but tracing still requires manual slot visitor
  boilerplate in VM types.
- persistent handle naming (`GlobalHandle`) is backend-oriented rather
  than user-oriented (`Root` / `Persistent`).

Task 94 turns those primitives into a stable contributor-facing layer.

## API Balance Policy

Use this decision rule when designing a GC/VM-facing API:

1. Start with a safe wrapper that carries the active context/session and
   performs rooting, barriers, and accounting automatically.
2. If the wrapper is too slow, benchmark first. Do not expose raw GC
   internals based on intuition.
3. If a low-level API is still necessary, keep it `pub(crate)` unless a
   real embedder/contributor use case requires it outside the crate.
4. If it must be public, mark it `unsafe`, document exact invariants,
   add compile-fail tests for common misuse, and provide a safe wrapper
   for the common case.
5. If an API is easy to use incorrectly, it is not production-ready even
   if it is technically sound.

## Target API Tiers

### 94.1 — Public safe contributor API

This is the only API normal builtin/module authors should need:

```rust
pub struct NativeCtx<'rt> { /* existing public native view */ }

impl<'rt> NativeCtx<'rt> {
    pub fn alloc<T: GcTrace>(&mut self, value: T) -> Result<Local<'rt, T>, Error>;
    pub fn root<T: GcTrace>(&mut self, value: Local<'rt, T>) -> Result<Root<'rt, T>, Error>;
    pub fn weak<T: GcTrace>(&mut self, value: Local<'rt, T>) -> Result<Weak<'rt, T>, Error>;
    pub fn with<T: GcTrace, R>(&self, value: Local<'rt, T>, f: impl FnOnce(&T) -> R) -> R;
    pub fn with_mut<T: GcTrace, R>(
        &mut self,
        owner: Local<'rt, T>,
        f: impl FnOnce(&mut T, &mut GcMutator<'rt>) -> R,
    ) -> R;
}
```

The exact lifetime names depend on task 93. The invariant is that
contributors do not call `GcHeap::alloc`, `write_barrier_raw`,
`read_payload`, or `with_payload` directly unless they are working in
audited VM internals.

### 94.2 — Internal VM API

VM internals may use lower-level primitives, but they should still be
structured:

- `RuntimeCx` owns the active mutator turn.
- `GcMutator` / `GcSession` exposes allocation, rooting, and mutation.
- `HeapSlot<T>` / `GcField<T>` wrappers perform write barriers on
  assignment.
- `EscapableLocal` is the only way to return a local out of a nested
  handle scope.
- manual barrier functions are `pub(crate)` or `#[doc(hidden)]` and
  linted by `grep`/compile-fail gates.

### 94.3 — Unsafe collector backend API

These stay in `otter-gc` internals unless a task explicitly opens them:

- `RawGc`;
- `TraceTable`;
- raw slot visitors;
- page/space/scavenger/marking internals;
- direct handle-table mutation;
- raw pointer compression/decompression helpers;
- manual card marking / insertion barrier entry points.

If a public wrapper needs one of these, put the unsafe call in one
small adapter with a `# Safety` docstring and targeted tests.

## Handle Model

Implement and document four user-visible handle tiers:

1. `Local<'gc, T>`: temporary rooted handle, bound to a handle scope /
   mutator turn.
2. `EscapableLocal<'outer, T>` or equivalent: explicit return path from
   nested scopes.
3. `Root<'iso, T>`: persistent isolate-owned root for embedders, async
   host state, timers, module caches, and native objects.
4. `Weak<'iso, T>`: weak handle whose `upgrade` requires a matching
   `GcSession<'iso, '_>`.

Rules:

- `Gc<T>` is a raw-ish VM value, not a persistence API.
- `Root` is move-only or clone-explicit; cloning must be visible in
  diagnostics.
- `Root::get` returns a scoped `Local`, not an unrooted long-lived
  pointer.
- weak callbacks / finalization enqueue work; they do not run JS inside
  GC.

## Trace Ergonomics

Add a contributor-friendly derive or macro path:

```rust
#[derive(GcTrace)]
#[gc(type_tag = "ObjectBody")]
struct ObjectBody {
    #[gc(trace)]
    prototype: Option<GcField<ObjectBody>>,
    #[gc(trace_values)]
    properties: IndexMap<PropertyKey, Value>,
    #[gc(skip)]
    shape_id: ShapeId,
}
```

Requirements:

- normal VM/builtin types implement safe `GcTrace`, not unsafe
  `Traceable`;
- field attributes make traced vs skipped slots explicit;
- unsupported fields fail at compile time with a useful error;
- generated code routes all child slots through the official visitor;
- manual unsafe `Traceable` impls are allowed only inside `otter-gc` or
  explicitly audited VM files.

## Mutation / Barrier Ergonomics

Contributors should not remember to call a write barrier after every
pointer store. Provide one of:

- `GcField<T>::set(&mut self, cx: &mut GcMutator<'_>, value: Gc<T>)`;
- `HeapSlot<Value>::write(&mut self, cx, value)`;
- container wrappers for arrays/maps/sets that barrier on insert,
  replace, remove, resize, and clear.

Validation must include a negative grep/lint gate: no new product-code
manual barrier calls outside the approved modules.

## External / Backing Store Accounting

Expose a safe RAII token for memory not stored inline in GC cells:

```rust
pub struct ExternalMemory {
    bytes: u64,
    heap: HeapId,
}

impl NativeCtx<'_> {
    pub fn reserve_external(&mut self, bytes: u64) -> Result<ExternalMemory, Error>;
}
```

Use this for strings, array elements, typed-array backing stores, map /
set buckets, module source text, and host-owned buffers. Dropping or
resizing the token releases/reserves bytes. This mirrors V8's embedder
external-memory accounting concept while staying RAII-safe in Rust.

## Documentation Deliverables

Add examples for:

- writing a leaf GC object;
- writing an object with child `Value` slots;
- mutating a property/array element with automatic barriers;
- storing a long-lived root in a native module;
- creating and upgrading a weak handle;
- accounting a backing store;
- returning a value from a nested handle scope;
- what **not** to do: raw `Gc`, raw barrier, context-free weak upgrade,
  crossing worker/async boundaries.

These examples belong in `docs/book` once the API names stabilize. Task
94 is not complete until the book has the contributor-facing workflow,
not just internal design notes.

## Validation gates

- [ ] Public rustdoc for `otter-gc` starts with the safe API, not the
  page/space/raw internals.
- [ ] `docs/book/src/engine/gc-api.md` documents the stable safe path
  and links to rustdoc for exact signatures.
- [ ] `cargo test -p otter-vm --test compile_fail` rejects raw GC
  handles crossing async/worker/session boundaries.
- [ ] Doctests cover each Documentation Deliverables example.
- [ ] `grep -R "write_barrier_raw\\|RawGc\\|TraceTable" crates-next/otter-{runtime,modules,web} crates-next/otter-vm/src`
  has only allowlisted hits.
- [ ] At least one builtin and one hosted module are ported to the safe
  contributor API as reference implementations.
- [ ] `cargo test -p otter-gc -p otter-vm -p otter-runtime` green.

## Closing

Tick task 94 in [70-gc-master-tracker.md](./70-gc-master-tracker.md).
Update `AGENTS.md` if the contributor workflow changes.
