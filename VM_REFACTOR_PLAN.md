# VM Root-Aware Allocation Roadmap

Scope: active workspace crates under `crates/*`. Legacy crates under
`crates-legacy/*` and `REPL_DX_DESIGN.md` are out of scope.

This file tracks the current root-aware VM architecture and the remaining
blockers. It is not a diary. Landed work belongs here only as architecture
state or as a constraint that future work must preserve.

## Current Rule

Root-aware young allocation/reservation is the default for active VM paths when
the caller can expose live roots.

- Bytecode/interpreter paths with a live frame stack should use stack-rooted
  allocation/reservation helpers.
- Stackless synchronous VM re-entry paths should use runtime-rooted helpers and
  explicitly root local `Value`s plus argument/result buffers.
- Native bindings should allocate/grow/reserve through `NativeCtx` helpers.
- Native bindings that create captured native functions should use the
  `NativeCtx` captured-native allocation helper so captures, `this`,
  `new.target`, and caller-provided values stay visible during native-function
  metadata allocation.
- Contributor-facing object/surface builders created from `NativeCtx` must carry
  the native raw root slots plus cloned `this` / `new.target` roots across later
  method/accessor native-function allocation; allocating the object shell through
  `NativeCtx` is not enough if the builder installs functions afterwards.
- Intrinsics should allocate/grow/reserve through `IntrinsicArgs` helpers.
- Bootstrap constructor/prototype/native-metadata setup should use explicit
  bootstrap roots for the global object, in-construction objects, and any
  previously allocated constructor/prototype values.
- Runtime class installation and timer global installation must carry runtime
  or global roots through JS surface builders; builder shell allocation,
  native-function metadata allocation, and class-constructor body allocation are
  one rooted construction contract.
- Active bytecode helper modules should not hide allocation behind heap-only
  convenience APIs when the interpreter stack is available; allocate through the
  interpreter/root contract first, then populate the shape.
- Heap-only object/array/iterator/promise/error helpers are allowed only where
  roots are already held by handle/global tables or where the missing root
  contract is listed below.

## Current Architecture Snapshot

These are the constraints future VM work should treat as normal engine shape:

- Production VM code no longer exposes public rootless object/array allocation
  wrappers. Active object/array creation goes through interpreter stack roots,
  runtime roots, `NativeCtx`, `IntrinsicArgs`, or bootstrap/global roots.
- Public collection mutators route storage growth through rooted reserve
  helpers. `Map`/`Set` object keys are stored as traced `Value`s instead of
  moving-address hash keys, so nursery relocation rewrites live object keys.
- WeakMap/WeakSet use the GC ephemeron trace contract. Weak keys do not become
  strong roots; WeakMap values are traced only after their key is live through
  ordinary reachability. Young weak keys and ephemeron registry slots are
  updated or pruned during scavenging.
- WeakRef and FinalizationRegistry active constructors allocate through
  stack/NativeCtx roots. Weak registry slots are fixed up during young
  collection without treating weak targets as roots.
- Native function metadata, captured-native allocation, Promise capabilities,
  Promise combinator buffers, iterator state, bound functions, proxy revocable
  objects, namespace builders, class/global surface builders, and timer globals
  now carry explicit roots across allocation.
- Runtime structured-clone tests build VM values through real JS execution.
  Cross-crate tests do not call raw VM heap setup helpers.
- Raw old-space setup helpers exist only under `#[cfg(test)]` inside
  `otter-vm`. The old task-numbered `gc_phase1_regressions` integration target
  was replaced by invariant/domain unit tests under `gc_invariants`, with no
  ignored placeholder tests.

## Non-Goals For This Plan

- Do not touch legacy crates.
- Do not touch `REPL_DX_DESIGN.md`.
- Do not add `unsafe` outside `crates/otter-gc`.
- Do not touch computed `LoadElement`/`StoreElement` IC.
- Do not start JIT work.
- Do not start polymorphic/megamorphic IC work.
- Do not start benchmark-batch work from this file.

## P0 - Remaining Root Contract Blockers

### Binary, TypedArray, ArrayBuffer

Current status:

- Active SharedArrayBuffer constructor dispatch uses rooted external-memory
  accounting for fixed and growable backing stores. Growable SAB reserves
  `maxByteLength` up front, matching its reserved vector capacity.
- Active non-shared `ArrayBuffer` and TypedArray constructor/copying prototype
  paths use rooted external reservation helpers.
- ArrayBuffer, SharedArrayBuffer, TypedArray, and DataView JS surface
  constructor/prototype/native-metadata bootstrap is rooted and is no longer a
  compatibility fallback surface.
- SharedArrayBuffer backing stores hold a GC-side shared external-memory token.
  The token can drop from another thread and reports released bytes into
  heap-owned atomic state; the owning heap drains those releases on later
  accounting/stat calls.
- `DataView` currently publishes an `Rc` view over an existing `ArrayBuffer`
  and does not allocate or reserve backing storage itself. Revisit only if the
  view body becomes GC-managed or gains external ownership.

Remaining problem:

- Low-level `JsArrayBuffer::new` / `from_bytes` callers may remain only for
  tests or future shared-buffer ownership helpers. Active backing-store
  allocation must stay on rooted external-reservation paths.

Target shape:

- Keep low-level buffer ownership rules centralized in the binary module.
- Revisit only if SAB grows backing storage lazily instead of reserving
  `maxByteLength` up front, or if future agent transfer exposes additional
  ownership edges.

Tests:

- TypedArray/ArrayBuffer construction roots source values and pending backing
  storage across reservation-triggered GC.
- SharedArrayBuffer fixed/growable construction accounts backing storage and
  releases it when the final shared backing body drops, including release
  notification from another thread.

### Runtime-Owned Host Objects

Current status:

- Runtime extension/native contributor surfaces for ordinary object,
  host-object, array construction, and object builder method/accessor
  installation now route through `NativeCtx`; remaining direct `gc_heap_mut()`
  uses in `otter-runtime/src/surface.rs` are non-allocating mutation/borrow glue
  or interpreter-owned bootstrap builder setup.
- The rootless `object::alloc_host_object` compatibility wrapper has been
  removed; host-data object allocation goes through runtime/interpreter/native
  root contracts or focused object-module tests that call the rooted helper
  explicitly.
- Diagnostic OOM throwable allocation remains a narrow old-space cap-bypass
  escape hatch so heap-cap failures can become catchable `RangeError`s; the
  helper is crate-private and drains pending shared-external releases before
  recounting heap pressure.
- Dynamic import now creates `import_meta` through the rooted host-object
  helper and populates `import_meta.url` for both on-demand `file://` modules
  and fetched HTTP(S) modules before running the module initializer.
- Module env/import-meta allocation should stay on the host-rooted interpreter
  helpers; do not reintroduce raw heap allocation in module loading paths.

Remaining problem:

- Some diagnostic/runtime helper objects still use generic allocation because
  they are created outside a VM frame stack and do not yet expose a complete
  runtime root contract.

Target shape:

- Define runtime-rooted allocation contracts for host-created JS objects.
- Root module graph/runtime state values, import metadata strings, namespace
  bindings, diagnostic values, and pending object fields across allocation.

Tests:

- Import-meta and module namespace creation preserve pending strings/bindings
  across allocation-triggered GC.
- Runtime diagnostic throwable/object creation is covered by root-aware tests
  or explicitly remains blocked by the native error slice.

### Native Function Metadata And Property Bags

Current status:

- The remaining heap-only ordinary function property-bag helper is used by
  computed `StoreElement` paths. Leave it until the computed
  `LoadElement`/`StoreElement` slice is allowed to change.
- Detached iterator method synthesis (`it.next`, `it.return`, `it.throw`) now
  uses stack-rooted captured native-function allocation.
- Native `Proxy.revocable` revoke-function/result-object creation now uses
  `NativeCtx` rooted captured-native/object allocation.
- WeakMap/WeakSet active insertion paths now use rooted storage reservation:
  bytecode `NewCollection` seed insertion, intrinsic prototype methods, and
  native bootstrap prototype methods route through stack/IntrinsicArgs/NativeCtx
  contracts.
- `Map`/`Set` object-shaped keys no longer use moving heap addresses as their
  hash/equality projection. Key slots are stored as traced `Value`s, so young
  object keys are rewritten during minor collection and iteration returns the
  relocated live value. Public collection mutators now route all storage growth
  through rooted reserve helpers; the rootless collection reserve helpers have
  been removed.
- WeakMap/WeakSet entry keys stay weak but now participate in the GC's
  ephemeron trace contract during minor collection. Young weak keys are updated
  only when already live through ordinary reachability; WeakMap values become
  strong only for such live keys. Weak collection storage uses rewrite-safe
  linear entries instead of hash buckets keyed directly by moving `RawGc`
  offsets.
- WeakRef and FinalizationRegistry active constructor paths use rooted
  allocation contracts through stack/NativeCtx helpers. FinalizationRegistry
  rooted allocation explicitly traces the cleanup callback until the registry
  body is installed. Young-generation scavenging now updates forwarded
  weak-finalization and ephemeron registry entries without making them roots.
  Public heap-only WeakRef/FinalizationRegistry wrappers have been removed from
  the production module API. Raw mark/sweep edge coverage lives inside
  `otter-vm` unit-test modules so old-space setup hooks are not part of the
  downstream VM API.
- The historical task-numbered GC regression target has been removed. Remaining
  coverage is named by invariant/domain under `gc_invariants`, and runtime
  structured-clone tests now create VM values by running JS through the active
  interpreter instead of calling raw heap setup helpers from another crate.
- Hosted-module namespace builders now allocate their namespace object through
  interpreter runtime roots and carry those raw runtime roots across later
  method/accessor native-function installation.
- Builtin namespace bootstrap for Math, JSON, Atomics, Reflect, and console now
  uses namespace builders with explicit global roots instead of heap-only
  namespace object/method allocation.
- Class/global JS surface installation now carries runtime roots and the global
  object through constructor/prototype/static object allocation, native metadata
  allocation, and `ClassConstructorBody` allocation. Bytecode `MakeClass` now
  allocates the class-constructor body with the interpreter stack root set.
- Timer global function installation now roots `globalThis` while native
  metadata is allocated.
- Object internal descriptor lookup no longer exposes a rootless
  `ordinary_get_own_property_descriptor_value` helper; proxy invariant paths
  use runtime-rooted descriptor lookup and explicitly root target/local
  descriptor values plus live key vectors.
- Ordinary function user-property bag allocation no longer has a heap-only
  helper. Computed function property writes allocate the bag through the live
  interpreter stack and root receiver/key/value state while the bag is created.
- The unused `FunctionBuilder` compatibility surface has been removed; native
  function installation now goes through rooted object/constructor/class/
  namespace builders or direct rooted native allocation helpers.
- Array/collection `@@iterator` factory creation still uses heap-only captured
  native function helpers from the computed `LoadElement` symbol path; this is
  blocked by the current "do not touch computed LoadElement/StoreElement IC"
  constraint.

Target shape:

- Move Array/collection `@@iterator` factory creation off the heap-only captured
  native function helper when computed `LoadElement` symbol dispatch is in
  scope.

Tests:

- Array/collection `@@iterator` factory allocation roots captured/native function
  values and pending symbol-dispatch state.

## P1 - Internal Compatibility Surfaces To Remove

Remove old helper variants only after the last active caller is migrated.

- Direct `gc_heap_mut()` use near allocation, growth, or reservation.
- Heap-only error/descriptor builders that are only used by active VM internals
  or tests.

Already removed from production/developer surface:

- Public low-level rootless `object`/`array` allocation wrappers.
- Public heap-only WeakRef/FinalizationRegistry wrappers.
- Rootless collection reserve helpers.
- Unused `FunctionBuilder` compatibility surface.
- Cross-crate `otter_vm::test_support` raw heap hooks.
- Task-numbered GC integration target and ignored placeholder GC tests.

Allowed temporary callers:

- Bootstrap-only static setup only when the values are already rooted by a
  realm/global table before the allocation can trigger GC.
- Tests that explicitly construct fixtures and do not assert fallback
  semantics.
- External public/runtime APIs whose root contract is not yet designed.
- Computed `LoadElement`/`StoreElement` paths while that IC slice is explicitly
  out of scope.

Expected cleanup rule:

- Do not leave an internal compatibility wrapper just because it used to exist.
  If all production active callers moved, delete the wrapper and update tests.

## P1 - Frame And Root Scanning Work

Remaining problem:

- `Frame` still carries hot registers plus cold async/exception/generator/
  iterator/bind state.
- Parked async/generator frames make root scanning and allocation contracts more
  expensive than they need to be.

Target shape:

- Split hot frame state from cold side records.
- Move register storage toward a contiguous VM value stack with frame base and
  register-count metadata.
- Make parked async/generator state root scanning explicit and smaller.

Do not start this as part of small allocation cleanup slices. It is a larger
VM-shape refactor.

## P1 - Object Storage Follow-Ups

Remaining problem:

- Delete marks objects dictionary-compatible, but storage is still shape/vector
  based.
- ICs deliberately cover only monomorphic ordinary fast-shape string-keyed data
  slots.

Target shape:

- Add dictionary storage mode for mutated/delete-heavy objects.
- Keep monomorphic IC semantics stable before considering broader IC forms.
- Do not add polymorphic/megamorphic ICs from this plan.

## P2 - Budget Scheduling Follow-Ups

Remaining problem:

- Runtime budget limits can observe and optionally reject, but cooperative
  yield/requeue is not implemented.

Target shape:

- Define resumable VM turn boundaries.
- Add host/runtime scheduling policy for cooperative yielding.
- Preserve existing structural `BUDGET_EXCEEDED` rejection semantics.

## Search Checklist For Next Allocation Slices

Use fff first in this git-indexed repository.

Search active crates, excluding legacy:

- `object::alloc_object(` (should have no production active hits)
- `object::alloc_object_with_proto(` (should have no production active hits)
- `array::alloc_array(` (should have no production active hits)
- `array::from_elements(` (should have no production active hits)
- `gc_heap_mut()`
- `descriptor_to_object`
- `reserve_external`
- `reserve_bytes`
- `test_support` (must stay crate-internal to `otter-vm` tests)
- `#[ignore` under `crates/otter-vm/src/gc_invariants`

For each hit:

- Ignore tests unless they assert old fallback behavior.
- Ignore true bootstrap-only static setup.
- If the path has a VM stack, migrate to stack-rooted allocation.
- If the path has runtime state but no VM stack, migrate to runtime-rooted
  allocation.
- If the path is native/intrinsic, migrate through `NativeCtx` or
  `IntrinsicArgs`.
- If no root context exists, add the smallest blocker note above instead of
  adding a fallback wrapper.

## Validation Checklist

For code changes in this plan:

- `cargo fmt --all`
- `cargo check -p otter-vm`
- `cargo test -p otter-vm --lib`
- `cargo check -p otter-runtime`
- `cargo test -p otter-gc` when root/GC contracts changed
- `git diff --check`
- unsafe scan outside `crates/otter-gc`; expected remaining hit is only
  `crates/otter-modules/src/ffi.rs` unless FFI is in scope
- relevant `cargo test -p otter-runtime --test ...` when runtime behavior is
  touched
