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
- [ ] First `AllocStub` runtime stub with GC-stress coverage.
- [ ] JIT call path to stubs without `NativeCtx`.
- [ ] Map/Set feedback model.
- [ ] Compiled `Map.get` / `Map.has` hot loop.
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
