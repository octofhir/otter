# Native VM ABI Plan

**Created:** 2026-06-27
**Status:** active architectural refactor plan

This document tracks the aggressive VM-native refactor needed to move Otter from
"Rust VM with JIT fast paths" toward a production JavaScript engine architecture
closer to JSC/V8: tiered native execution, low-friction runtime stubs, precise
safepoints, and minimal Rust boundary crossings on hot paths.

The goal is not backward compatibility with the current internal ABI. Breaking
changes are allowed when they simplify the execution model, improve peak
throughput, and preserve ECMAScript conformance.

## Thesis

Otter's remaining 8-10x gap on non-numeric workloads is structural. The hot loop
is still too often a bytecode interpreter loop plus Rust runtime calls. JSC and
V8 win by compiling the whole hot region:

- receiver/prototype guards;
- string concatenation and hashing;
- Map/Set probes;
- property and method inline caches;
- allocation fast paths and write barriers;
- safepoints/deopt metadata.

The key architectural shift is to make the interpreter, baseline JIT,
optimizing JIT, runtime stubs, and GC agree on one native execution ABI.

## External Reference Model

Primary references:

- JSC tiering: LLInt -> Baseline JIT -> DFG -> FTL.
  <https://docs.webkit.org/Deep%20Dive/JSC/JavaScriptCore.html>
- JSC speculation and OSR exits:
  <https://webkit.org/blog/10308/speculation-in-javascriptcore/>
- V8 Sparkplug baseline compiler:
  <https://v8.dev/blog/sparkplug>
- V8 Maglev mid-tier optimizer:
  <https://v8.dev/blog/maglev>
- V8 hidden classes and object layout:
  <https://v8.dev/docs/hidden-classes>
- V8 generational/incremental/compacting GC background:
  <https://v8.dev/blog/trash-talk>

Lessons to copy in spirit, not literally:

- Baseline compilation exists because interpreter dispatch and operand decode
  have a hard throughput ceiling.
- Optimizing tiers compile across the whole loop body, not just arithmetic.
- Speculation is made correct by exact deopt frame states and dependency
  invalidation, not by replaying functions from entry.
- Moving GC is compatible with native code when safepoints and stack maps are
  first-class ABI objects.
- Builtins on hot paths are runtime stubs/intrinsics, not generic host calls.

## Current Structural Friction

### 1. Interpreter frame is not the engine ABI

The current interpreter stack is correct and reservation-stable, but it is still
an interpreter-owned abstraction. Native code must adapt to it instead of sharing
a single frame convention with it.

Symptoms:

- deopt and OSR require translation between machine state and interpreter
  register windows;
- runtime calls must reason about Rust-owned `Interpreter`, `HoltStack`, and
  `ExecutionContext` objects;
- compiled code is incentivized to avoid GC-bearing values in machine registers
  rather than describe them precisely.

### 2. Native boundary is too generic

The generic native path roots receiver/arguments/interpreter state, builds a
`NativeCtx`, calls Rust, maps errors, and tears down roots. Fast collection
dispatch reduced the worst overhead, but the model is still per-call boundary
management.

Hot builtins need ABI classes:

- `LeafNoAlloc`: cannot allocate, cannot call JS, cannot observe safepoints;
- `AllocStub`: may allocate and therefore owns a precise safepoint map;
- `ReentrantStub`: may call JS/proxy/accessors and must support full deopt/root
  protocol.

### 3. GC safepoints are not the central contract yet

The moving collector is the right long-term choice, but hot native execution
needs stack maps and safepoints instead of ad hoc root scopes around every call.

Required default contract:

- no GC at `LeafNoAlloc` sites;
- exact tagged-value maps at every allocation, runtime call, and deopt guard;
- inline allocation fast path plus slow path that consumes the current safepoint;
- inline or stubbed write barriers;
- GC-stress validation of every compiled safepoint.

### 4. Exotic objects are outside the ordinary object/IC model

Map, Set, Array, RegExp, Temporal, and related values are distinct body types
instead of ordinary JS objects with internal slots. This leaks into every IC,
method dispatch, and JIT guard design.

Target model:

```text
JSCell / object header
  class id / type flags
  shape pointer
  prototype pointer
  slot storage pointer
  exotic payload pointer
```

Map/Set keep specialized payloads, but method lookup, prototype guards, expando
properties, and JIT object checks use the same object protocol as ordinary
objects.

### 5. Bytecode is not tiering-friendly enough

`ExecInstr` has good decoded semantics for the interpreter, but hot native tiers
want stable compact metadata:

- instruction-index PCs for hot frame state;
- dense feedback vector slots;
- bytecode-level safepoint/call/deopt annotations;
- fixed-width hot operand views;
- quickened opcodes for stable monomorphic sites.

## Target ABI

### JS frame layout

Every JS frame, interpreted or compiled, should be describable by the same
metadata:

```text
FrameHeader
  previous frame pointer / stack link
  function id
  code block id
  resume bytecode pc or instruction index
  frame flags
  register count
  argument count
  feedback vector pointer/index
  caller-saved runtime scratch

RegisterWindow
  tagged Value slots visible to deopt, debugger, profiler, and GC

NativeSpillArea
  tier-owned machine spills, described by safepoint maps when live
```

Rules:

- interpreter and JIT both materialize frame headers in this shape;
- deopt reconstructs the register window directly, not by replaying function
  entry;
- debugger/profiler/stack trace use the same frame walk;
- JIT code may keep unboxed values in registers between safepoints;
- every safepoint maps tagged values in registers/spills/frame slots.

### Runtime stub ABI

Runtime stubs should be explicit machine-callable functions, not generic
`NativeCtx` calls on hot paths.

```text
StubResult
  status: ok | throw | deopt | oom | interrupt
  value: Value
  side payload: error/deopt id when needed
```

Stub inputs:

- isolate/runtime pointer;
- current frame pointer;
- safepoint id for allocating/reentrant stubs;
- raw `Value` arguments in ABI registers or frame slots;
- feedback/dependency id where relevant.

Stub classes:

- `LeafNoAlloc`: no safepoint id, no root registration, no VM reentry;
- `AllocStub`: safepoint id required, may trigger scavenge/full GC;
- `ReentrantStub`: safepoint id and deopt continuation required.

Initial candidate stubs:

- `Map.get`, `Map.set`, `Map.has`, `Set.add`, `Set.has`;
- string concat, flatten, hash, `charCodeAt`;
- dense array length/load/store/push;
- shape load/store IC miss handlers;
- allocation slow path and write barrier slow path.

### Safepoint maps

Safepoint metadata must cover:

- JS frame header location;
- tagged frame slots;
- tagged machine registers;
- tagged spill slots;
- deopt frame-state id;
- call continuation PC;
- stub class and allocation/reentry permissions.

The same metadata should serve:

- moving GC root discovery and update;
- deopt materialization;
- profiler/debugger stack walking where possible;
- GC-stress validation.

## Implementation Phases

### Phase 0: ABI inventory and measurement

Status: in progress. The first passive ABI descriptors and class counters are in
place for the current JIT -> VM transitions. Baseline, dynasm optimizing, and
Cranelift optimizing compiled backedges now use a named leaf runtime-stub ABI
entry for interrupt and runtime-budget polling, so timeout semantics no longer
depend on whether a loop has OSR'd. Full frequency profiling is still open.

Tasks:

- document every current transition:
  interpreter -> native, JIT -> VM, JIT -> runtime, runtime -> JS callback,
  allocation slow path, write barrier slow path, deopt, OSR;
- count hot transition frequency on `map-set.js`, `string-ops.js`,
  `tree-traversal.js`, `prop-access.js`, `array-ops.js`;
- add counters for root-scope pushes, native calls by builtin, allocating
  builtin calls, safepoint-like calls, deopts, OSR entries, IC misses;
- establish per-op interpreter cost and per-stub-call cost on Apple Silicon.
- keep every compiled loop on the same interrupt/runtime-budget contract as the
  interpreter.

Exit criteria:

- a transition table exists with frequency, inclusive time, and owner;
- each top benchmark has a measured dominant boundary or proof that it is
  compiler coverage instead.

### Phase 1: Define native frame and safepoint metadata types

Status: not started.

Tasks:

- add VM-owned ABI structs for frame header, frame kind, safepoint record,
  deopt frame-state link, and runtime stub result;
- keep them passive at first, with no behavior change;
- make baseline/optimizing code emission and interpreter frame code reference
  the same metadata definitions;
- add tests for layout size/alignment and serialization/debug dumps.

Exit criteria:

- one source of truth exists for JS frame/safepoint/deopt metadata;
- no new JIT/runtime feature can invent a private frame convention.

### Phase 2: Runtime stub ABI v1

Status: started. `Map.get`, `Map.has`, and `Set.has` now have explicit
`LeafNoAlloc` ABI descriptors and a guarded no-root/no-GC dispatch path for keys
that are already flat/hashable. Leaf probes return the shared
`RuntimeStubResult` (`Ok(Value)` / `Miss`) instead of a local Rust-only shortcut,
and live in `runtime_stubs` as reusable raw-`Value`-bits entrypoints instead of
being embedded in interpreter method dispatch. Each leaf probe now has a
callable ABI entry pairing its descriptor with a typed `LeafNoAllocStub2`
function pointer, so the same result/call ABI can be reused by future direct JIT
calls. Method-call feedback now carries the resolved leaf `RuntimeStubId`, not
just the high-level collection operation, and `runtime_stubs` provides a single
descriptor-id lookup/invocation path for fixed two-argument leaf entries.
Leaf entries now use an explicit native ABI (`extern "C"` heap pointer plus raw
boxed value bits) rather than Rust's default function ABI, and a generic
`leaf_no_alloc_stub2_trampoline` lets generated code call a dynamic
`RuntimeStubId` before later specializing to a direct entry address. Leaf
results also have a two-register `RuntimeStubResultPair` form so generated code
does not need a hidden structure-return pointer just to inspect `Ok` / `Miss`,
and `JitCtx` now carries an opaque heap pointer for direct leaf calls. Compiled
code still reaches this through the current method runtime stub; baseline
compiled `CallMethodValue` now tries a narrow collection-leaf bridge before the
generic method bridge, so hot `Map.get` / `Map.has` / `Set.has` sites can return
through the reusable leaf ABI without building the full method-call argument
path. Fully direct machine calls to the leaf entries remain open.
`Map.prototype.set`, `Set.prototype.add`, materializing collection lookups
(`Map.get`, `Map.has`, `Set.has`), and materializing collection deletes
(`Map.delete`, `Set.delete`) now have concrete `AllocStub` descriptors with a
uniform three-value ABI shape (`receiver`, `arg0`, `arg1_or_undefined`).
`runtime_stubs` can resolve these descriptor ids as allocating ABI records and
validates that callers name a non-sentinel safepoint. JIT compile snapshots now
carry `JitCollectionAllocMethod` feedback for warmed collection method sites:
receiver/prototype/builtin guards plus the allocating stub id. The first
safepoint-map slice is now in place: `JitFunctionView` carries baked
`SafepointRecord`s keyed by
`SafepointId`, and `JitCollectionAllocMethod` names the safepoint to publish for
its call site. Baseline v1 records the full interpreter-visible register window
as tagged frame-slot roots, matching the current rooted frame model while leaving
finer liveness/register/spill maps for a later slice. The allocating-stub call
packet is also explicit now: `RuntimeStubAllocContext` carries only erased
VM/stack/context pointers, the current frame index, and the raw tagged frame-slot
window. The fixed `AllocValueStubFn` shape takes this context plus a safepoint id
and raw `Value` arguments, still without exposing or constructing a generic
`NativeCtx`. The packet now also names the active function's safepoint-record
table, so an allocating runtime stub can resolve its `SafepointId` inside the
machine ABI instead of trusting an out-of-band Rust argument. `runtime_stubs` can
validate and publish an allocating safepoint backed by that frame-slot window
through `AllocSafepointFrameRoots`, rejecting unsupported register/spill maps
until native frame locations are publishable. The allocating value-stub catalog
is no longer just passive metadata: `AllocValueStub` carries an optional
executable entrypoint and common entry-address/raw-invoke helpers. Collection
mutation and lookup stubs now have VM-side executable entries that consume the
same context packet and safepoint table that baseline code passes at warmed
Map/Set sites. Baseline compiled `CallMethodValue` keeps the existing
receiver/prototype/builtin guards, tries the `LeafNoAlloc` lookup first where
available, then builds a stack-local `RuntimeStubAllocContext` from `JitCtx`,
passes the compiled function's stable safepoint table and raw receiver/argument
value bits, and writes the relocated `Ok` result directly back to the
destination frame slot. `Miss` and other non-`Ok` statuses still fall through to
the existing rooted method fallback, while the fast path avoids the generic
`NativeCtx` bridge. String/array/property allocating stubs can plug into the same
ABI record instead of growing per-feature bridge shapes.

Tasks:

- introduce explicit stub classes: `LeafNoAlloc`, `AllocStub`,
  `ReentrantStub`;
- convert one non-allocating hot operation to `LeafNoAlloc`;
- convert one allocating hot operation to `AllocStub` with a safepoint record;
- keep semantic fallbacks through the existing Rust path;
- add GC-stress tests for the allocating stub.

Recommended first slice:

- `Map.has` / `Map.get` as leaf candidates when key is already flat/hashable and
  no user-observable fallback is needed;
- `Map.set` as allocating candidate after the leaf path is proven.

Exit criteria:

- optimized/baseline code can call at least one runtime stub without constructing
  `NativeCtx`;
- allocating stub calls root/update live values through safepoint metadata, not
  per-call `ExtraRoots`.

### Phase 3: Compiled loop owns Map/Set and string concat fast paths

Status: not started.

Tasks:

- add feedback for Map/Set receiver class, prototype identity, builtin identity,
  key representation, and string flatness/hash state;
- lower monomorphic `CallMethodValue` for Map/Set into guarded stub or inline
  probe nodes;
- lower `"k" + int` style string concat into a specialized string node with
  allocation fast path and deopt fallback;
- OSR into loops containing these nodes;
- invalidate on prototype/method/global dependency changes.

Exit criteria:

- `map-set.js` hot loop runs compiled after warmup;
- native-call count in the loop is zero for leaf hits or limited to explicit
  runtime stubs;
- interpreter dispatch is no longer the top profile frame.

### Phase 4: Object header unification

Status: not started.

Tasks:

- design a shared object header for ordinary and exotic objects;
- migrate Map/Set first behind compatibility accessors;
- move prototype/expando/shape-visible state out of bespoke collection bodies;
- make object ICs and method ICs work for Map/Set without bespoke receiver
  special cases;
- preserve tracing, write barriers, insertion-order semantics, and iterator
  behavior.

Exit criteria:

- `Value::as_object_like` can return a guardable shared object header for
  ordinary objects, arrays, maps, and sets;
- Map/Set method lookup uses the same IC/dependency model as ordinary objects.

### Phase 5: Interpreter quickening and bytecode metadata

Status: not started.

Tasks:

- add feedback-vector-indexed quickening for stable monomorphic sites;
- convert hot frame PC to instruction index while preserving byte-PC debug/deopt
  mapping;
- inline fixed hot operands or provide a compact hot operand view;
- move reduction checks to weighted blocks/backedges with interrupt polls at
  calls, loops, allocations, and explicit safepoints.

Exit criteria:

- interpreter cost approaches the practical Rust/direct-dispatch floor;
- interpreter remains a correct deopt/debug target but is no longer the primary
  performance strategy for hot loops.

## Progress Checklist

- [ ] Phase 0 transition inventory and counters.
- [x] Native frame/safepoint metadata structs.
- [x] Stub result and stub class ABI.
- [x] JIT backedge interrupt/runtime-budget poll stub.
- [x] First `LeafNoAlloc` runtime stub.
- [x] Baseline pair-result call sequence for collection `LeafNoAlloc` stubs.
- [x] JIT-readable collection method IC leaf guards.
- [x] AllocStub descriptor/call-shape scaffold for `Map.set` / `Set.add`.
- [x] Baseline frame-slot safepoint records for collection `AllocStub` sites.
- [x] Explicit `RuntimeStubAllocContext` and `AllocValueStubFn` ABI shape.
- [x] Safepoint table view in `RuntimeStubAllocContext`.
- [x] Frame-slot root publisher for `AllocStub` safepoints.
- [x] Executable-entry slot on generic `AllocValueStub` ABI records.
- [x] First VM-side executable `AllocStub` runtime stub.
- [x] Moving-GC coverage for executable `AllocStub` roots.
- [ ] JIT call path to stubs without `NativeCtx`.
- [x] Map/Set feedback model for leaf lookup stubs.
- [x] Compiled `Map.get` / `Map.has` hot loop.
- [ ] Compiled `Map.set` / `Set.add` hot loop.
- [ ] String concat specialized node.
- [ ] Shared object header design.
- [ ] Map/Set migration to shared object header.
- [ ] Interpreter quickening and block/backedge metering.

## Verification Contract

Every ABI slice must state which of these it touches:

- frame layout;
- deopt reconstruction;
- GC safepoints;
- runtime stubs;
- object layout;
- bytecode metadata.

Required checks scale with touched surface:

- `cargo test -p otter-vm`;
- `cargo test -p otter-jit`;
- targeted Test262 subset for touched builtins/opcodes;
- JIT on/off parity for touched benchmarks;
- `OTTER_GC_STRESS=128 OTTER_JIT=1` for any allocating compiled path;
- profile before/after on the triggering benchmark, including transition
  counters.

## Non-Goals

- No benchmark-specific semantic shortcuts.
- No conservative native stack scanning as the default GC strategy.
- No bail-to-function-entry deopt shortcuts.
- No per-feature runtime kill switches.
- No new public runtime boundary exposing raw `Rc`, `RefCell`, raw heap handles,
  or untracked GC pointers.

## Slice Notes

### 2026-06-27: Baseline collection leaf pair calls

Touched surface: runtime stubs and JIT/runtime ABI.

Baseline `CallMethodValue` now splits the collection leaf method path into two
steps: a small VM bridge validates the existing method IC guards and returns the
`RuntimeStubId`, then generated code reads `JitCtx.gc_heap`, loads receiver/key
raw `Value` bits from the frame register window, calls
`leaf_no_alloc_stub2_trampoline_pair`, checks the low status byte, and writes
the returned value bits directly to `dst` on `Ok`. `Miss` and any unexpected
non-`Ok` status continue through the existing direct/full method fallback.

Remaining bridge: receiver/prototype/builtin guard resolution is still in Rust.
Next slice should make method IC guard metadata JIT-readable so this resolver
bridge can be removed.

### 2026-06-27: JIT-readable collection leaf guards

Touched surface: runtime stubs, object/function layout metadata, and JIT/runtime
ABI.

Collection leaf method ICs now bake a `JitCollectionLeafMethod` DTO into the
compile snapshot. Baseline code validates the receiver family, no
prototype/expando override flags, prototype shape, method slot, and native
builtin identity directly in machine code before calling the pair-result leaf
stub. This removes the `jit_resolve_collection_leaf_method_stub` bridge from the
steady-state baseline path; misses fall through to the existing direct/full
method fallback.

The guard is deliberately stricter than the Rust IC for explicit prototype
overrides: even an override pointing back to the canonical prototype misses the
machine guard and uses fallback. That preserves semantics while keeping the
first machine-readable guard compact. The next ABI slice should turn
`Map.set`/`Set.add` into `AllocStub` entries with safepoint metadata.

### 2026-06-28: Collection AllocStub ABI scaffold

Touched surface: runtime stubs, GC safepoint ABI metadata, and JIT/runtime ABI.

`Map.prototype.set` and `Set.prototype.add` now have explicit `AllocStub`
descriptors with stable runtime stub ids and a fixed machine-call value shape:
`receiver`, `arg0`, `arg1_or_undefined`. The descriptor layer enforces that
allocating stubs cannot be validated with `NO_SAFEPOINT`, and `runtime_stubs`
exposes an `AllocValueStub` resolver record without an executable entrypoint. This
keeps the architecture moving without introducing an unsafe mutation fast path
before compiled frames can publish exact roots.

Collection method ICs now record the allocating stub id for `Map.set` /
`Set.add`, and compile snapshots can bake a `JitCollectionAllocMethod` DTO with
the same receiver/prototype/builtin guard metadata used by leaf methods. Baseline
still falls through to the existing rooted dispatch for actual allocation and
GC. The next slice should add a real safepoint record for baseline method-call
sites and only then attach a machine-callable allocating entry.

### 2026-06-28: Collection AllocStub safepoint records

Touched surface: GC safepoint ABI metadata and JIT/runtime ABI.

Allocating collection method feedback now carries a `SafepointId`, and
`JitFunctionView` includes the corresponding `SafepointRecord`. For baseline v1
the record maps every interpreter-visible register in the frame window as a
tagged `FrameSlot` root. This is deliberately wider than final liveness maps but
it is precise about storage class and keeps moving-GC correctness tied to the
shared frame-window root model before any machine-callable mutation stub is
enabled.

The next slice can use this metadata to publish the active safepoint around an
allocating runtime-stub call. Only after that should `collection_map_set_alloc`
or `collection_set_add_alloc` grow an executable entrypoint.

### 2026-06-28: AllocStub call context packet

Touched surface: runtime stubs, GC safepoint ABI metadata, and JIT/runtime ABI.

`RuntimeStubAllocContext` defines the C-layout context packet for allocating
runtime stubs. It carries the erased VM reentry pointers, current frame index,
and raw tagged frame-slot window, giving a future stub enough information to
publish/update roots through the safepoint map without constructing `NativeCtx`.
`runtime_stubs` now names the concrete `AllocValueStubFn` entry shape:
`(alloc_ctx, safepoint_id, receiver_bits, arg0_bits, arg1_bits) ->
RuntimeStubResultPair`.

This remains an ABI scaffold only. No `Map.set` / `Set.add` executable entry is
installed until generated code publishes the safepoint around the call and GC
stress can validate moving-root updates.

### 2026-06-28: AllocStub frame-slot root publisher

Touched surface: runtime stubs and GC safepoint ABI metadata.

`runtime_stubs` now has `AllocSafepointFrameRoots`, an `ExtraRootSource` backed
by `RuntimeStubAllocContext.frame_slots` plus a `SafepointRecord`. The validator
requires a concrete safepoint id, a non-empty frame-slot window, and frame-slot
locations within bounds; register and spill-slot maps deliberately fail until
the native frame layout can publish those locations. This makes the allocating
stub ABI root/update live frame values through safepoint metadata without
constructing `NativeCtx`.

This is still not an executable `Map.set` / `Set.add` fast path. The next slice
should use this publisher inside the concrete collection `AllocValueStub` entries,
then add GC-stress coverage before baseline machine code starts calling them.

### 2026-06-28: Generic AllocValueStub entry records

Touched surface: runtime stubs and JIT/runtime ABI vocabulary.

`AllocValueStub` now mirrors the leaf-stub catalog shape: it owns the passive
descriptor and, when implemented, a machine-callable `AllocValueStubFn`
entrypoint with shared `entry_addr` / `invoke_raw` helpers. This keeps the
allocation ABI engine-wide rather than collection-specific. `Map.set` /
At this point `Map.set` / `Set.add` still advertised no executable entrypoint,
so generated code could not accidentally call an allocating fast path before
exact-root GC stress coverage existed.

### 2026-06-28: AllocStub safepoint table view

Touched surface: runtime stubs, GC safepoint ABI metadata, and JIT/runtime ABI.

`RuntimeStubAllocContext` now carries a raw pointer/count view of the active
function's `SafepointRecord` table. `runtime_stubs` resolves `SafepointId`
through that table before publishing frame-slot roots, and reports missing-table,
`NO_SAFEPOINT`, and unknown-id cases explicitly. This removes the last
out-of-band Rust assumption from the `AllocValueStub` context packet: executable
allocating stubs can now consume the same machine ABI shape they will receive
from baseline code.

### 2026-06-28: Executable collection AllocValueStub entries

Touched surface: runtime stubs and GC safepoint ABI metadata.

`collection_map_set_alloc` and `collection_set_add_alloc` are now real
`AllocValueStub` entries. They resolve the safepoint id through
`RuntimeStubAllocContext`, publish frame-slot roots, root their ABI value copies,
flatten string keys/values under that root scope, call the existing collection
mutation helpers, and return the relocated receiver through
`RuntimeStubResultPair`. Invalid context/safepoint/receiver cases return `Miss`;
allocation failure returns `OutOfMemory`.

Baseline code still does not call these entries directly. The next required
slice is moving-GC coverage for the executable root protocol, followed by the
baseline machine-call path.

### 2026-06-28: AllocValueStub moving-root coverage

Touched surface: runtime stubs and GC safepoint ABI metadata.

`AllocValueStubCallRoots` now stores ABI value copies in `UnsafeCell<Value>` so
they are legitimate mutable root slots during a moving collection. Frame-slot
safepoint roots are also visited through mutable `Value` slots. The runtime-stub
tests now force a minor collection through the root publisher and verify that
both the caller frame slot and the stub-local ABI value copy are rewritten to
their relocated addresses.

With this coverage in place, the remaining performance slice is the baseline
machine-call path to these entries.
