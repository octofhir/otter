# Otter Engine Redesign

This is the sole repository-level implementation tracker. Otter is pre-user
and pre-stability: internal APIs, ABI, bytecode, metadata, artifacts, fixtures,
and tests may break whenever that produces the intended final architecture.
Completed slice history and measurements live in git and the ignored
`scratchpad/LEDGER.md`, not in this active plan.

## Objective

Replace every compiled execution path with one typed, target-neutral pipeline:

```text
bytecode + feedback
        |
        v
typed CFG HIR -> Machine IR -> target selection/legalization
        -> regalloc2 -> code + stack maps + deopt maps
```

Quick and optimizing tiers share HIR, Machine IR, target backends, frame
layout, call descriptors, safepoints, deopt reconstruction, dispatch cells,
and artifact schemas. They differ only in optimization budget and tier policy.

## Current state

The active replacement path is:

```text
typed scalar HIR -> Machine IR -> regalloc2 -> AArch64 emitter
```

It already owns:

- straight-line and reducible cyclic scalar CFGs, critical-edge splitting,
  block parameters, edge moves, instruction-exact liveness, and loop OSR;
- Tagged, Int32, Uint32, Float64, and Boolean values, including arbitrary
  tagged parameters, constants, locals, `this`, ordinary returns, tagged
  block parameters, tagged OSR inputs, checked arithmetic, and scalar VM leaves;
- descriptor-driven leaf calls for exact tagged truthiness and strict equality,
  including heap-cell semantics, scalar argument boxing, allocator clobbers,
  and exact miss deopt without a VM-window shuttle;
- descriptor-driven allocating primitive string concatenation with exact
  pre-operation deopt, allocator late-use roots, a reusable native root-save
  area, VM `SafepointRecord` spill locations, and post-GC reloads;
- descriptor-driven monomorphic plain, guarded-method, and fixed-arity base,
  derived, and superclass JavaScript calls from Machine IR through the shared
  generated-linkage emitter, with exact pre-call FrameState, allocator-owned
  receiver/arguments/results, own/prototype guards, inherited `new.target`,
  derived-`this` binding, and no interpreter-window shuttle;
- allocator-driven spills, AAPCS64 callee-saved allocation, exact frame sizing,
  fixed leaf ABI operands, and deterministic normalized allocation artifacts;
- `MachineFrameState -> lower_deopt_table -> VM DeoptTable`, shared cold exits,
  exact overflow PCs/windows, and backedge poll before phi moves;
- stack-owned nested `NativeFrame` publication, stable generation cells, and
  generated calls into template or optimizing callees; safepoint-free acyclic
  scalar generations initially publish only their initialized parameter
  prefix, while every cold exit expands the canonical VM window before reentry;
- caller-reserved upvalue spines for generated plain, guarded-method, fixed
  base-constructor, and spread linkage. Fresh callee cells are allocated into
  the spine before publication, inherited closure cells are appended exactly,
  and bounded two-edge target preparation closes an already-observed nested
  closure call without unbounded call-graph compilation;
- split base-constructor receiver preparation: exact class wrappers with an
  already materialized own data prototype allocate the complete receiver from
  a collector-published young from-space window in generated fixed, spread, and
  superclass linkage. Guard, page-capacity, marking, GC-stress, heap-cap, and
  OOM misses enter one rooted cold allocator before effects; accessors, proxies,
  bound functions, lazy prototypes, and uncertain shapes still enter the one
  observable fallback;
- final-shape receiver allocation for conservative straight-line base
  initializers. The shared fixed, spread, and generated-super preparation path
  proves every initializer name absent from the selected prototype chain,
  installs undefined own slots before entry, and lets the body overwrite them
  without StoreProperty shape-transition reentry;
- exact class-constructor field transitions at original `StoreProperty` sites. The
  VM retains movement-stable shape identities, the compile snapshot resolves
  live handles, and Machine IR guards the receiver plus the complete ordinary
  prototype chain before publishing shape, slab length, inline storage, and
  both GC barriers. Non-simple base and derived class fields therefore stay native
  without moving initializer effects earlier. The immutable plan and reserved
  capacity are cached per exact base/derived chain, so receiver allocation does
  not rescan prototypes after the guarded plan is installed;
- generated superclass lookup, derived-`this` binding, and base/derived return
  selection in Machine code. Materialized bind and invalid derived returns are
  cold siblings; fixed/spread direct arguments remain late tagged roots across
  receiver preparation, and result/type guards never replay a started callee.
  The replaced base-result stub and descriptor no longer exist;
- a VM-owned linked root chain for Machine values live across reentrant calls;
  return, constructor receiver preparation, callee overflow/deopt, propagated
  throw, nested/recursive generated calls, and moving minor GC all execute
  without replay or conservative stack scanning;
- exact generated receiver-allocation attribution for attempts, successes,
  structural and space misses, cold/Rust boundaries, GC, refills, OOM, and
  deopts, plus `directConstructReceiverAllocFast` / `Cold` code-map proof;
- complete legacy-optimizer splices consume dead numeric boxing at the removed
  call boundary while constructor descriptors inside the spliced body retain
  the shared generated linkage. Arguments, results, receiver roots, and
  moving-GC refreshes use the published inline-frame window rather than
  falling back to the generic construct transition;
- indexed Map/Set keys finish with a low-bit avalanche before the GC-owned
  ordered table selects a bucket. Adjacent integral doubles no longer collapse
  into one collision chain: the alternating `native-boundary` process medians
  fell by 59.02% with reductions, bytecode calls, JIT entries, deopts, runtime
  transitions, and emitted code held identical;
- optimizing guarded calls to `Math.abs`, `Math.max`, and `Math.min` complete
  directly in AArch64 when every operand is proven Int32. The exact bootstrap
  callee or receiver/prototype/method identity guard remains mandatory, and
  `abs(INT32_MIN)` materializes `2147483648` without crossing the Rust leaf ABI.
  Alternating long process medians fell by 31.47% for the isolated Math slice
  and 3.21% for the full `native-boundary` workload, with identical semantic
  checksums and transition totals;
- optimizing guarded primitive-string calls complete directly in AArch64 for
  `charCodeAt(Int32)` and single-code-unit `indexOf(String)` over contiguous
  inline or sequential Latin-1 / UTF-16 bodies. The existing exact prototype
  method identity guard remains mandatory; ropes, slices, coercive arguments,
  out-of-range reads, and searches longer than 256 code units enter the one
  canonical pre-effect fallback. Alternating long process medians fell by
  30.29% for the isolated `charCodeAt` slice, 39.85% for `indexOf`, and 11.70%
  for the full `native-boundary` workload with exact matching checksums and
  zero optimizing deopts;
- optimizing guarded `Map.get(Int32)` and existing-key
  `Map.set(Int32, value)` calls hash and probe the GC-owned ordered table
  directly in AArch64 after the exact receiver/prototype/method identity
  proof. Map entries now occupy one stable 32-byte record containing the
  original key, value, cached hash, chain link, and flags instead of retaining
  a second projected `MapKey`; generated overwrites run the shared table-parent
  write barrier only for cell values. Missing keys, `-0`/`+0` representation
  aliases, long chains, insertions, and structural drift enter the canonical
  pre-effect method path. Alternating 8-warmup/15-sample process medians fell
  by 64.54% for isolated `Map.get`, 65.74% for `Map.set`, 58.60% for their
  combined slice, and 39.13% for the full `native-boundary` workload with
  exact matching checksums and unchanged optimizing entry/deopt topology;
- optimizing outermost natural loops may cache multiple guarded Map, string, Math, and
  spliced-method sites in one activation. Invariant receivers reuse their
  validated body headers; varying exotic receivers revalidate the current body
  while sharing the pinned prototype identity proof. Every entry and OSR path
  starts empty, and any generated intrinsic miss clears all sites before the
  canonical allocating/reentrant transition. A coercive replacement fixture
  proves that a mid-loop `Map.prototype.get` mutation is observed after GC
  pressure. Alternating 10-warmup/25-sample process medians for a five-site
  Map/string/Math kernel fell by 13.08%, with exact matching checksum,
  7,073,851 reductions, 35 optimized entries, 1,708 runtime-stub transitions,
  and zero deopts; emitted native code grew by 348 bytes;
- activation-local method caches now survive generated dense-element,
  property-IC/exotic-length, and guarded global reads. Element and global
  eligibility requires a prepared allocation-free hit path; every semantic
  miss clears all raw method-cache slots before frame publication and generic
  reentry. The regression combines a global lexical array read, dense indexing,
  string length, five cached methods, and element/property accessor misses that
  allocate and replace `Map.prototype.get`. Alternating 10-warmup/25-sample
  process medians improved the existing mixed Map kernel by 15.61%, Map.set by
  11.77%, and Map.get-plus-length by 11.51%, with exact checksums, unchanged
  reductions/runtime-stub counts, and zero deopts. Native code grew by 120, 60,
  and 132 bytes respectively;
- primitive string constants now live in address-stable boxed cache cells.
  Compile snapshots publish only already-materialized traced cells; template
  and optimizing code read the live `Value` directly, while cold literals keep
  the canonical transition. Optimized `LoadString` no longer materializes a
  frame or safepoint, and loop method caches may cross the pure cell read. A
  257-cell VM regression proves address stability across hash growth and full
  GC; a production-tier regression executes compiled literal code after 300
  new eval chunks and moving collection. Typed `stringConstantCell`
  relocations redact the address. Alternating 10-warmup/25-sample medians
  improved the existing `indexOf` kernel by 53.33% and full `native-boundary`
  by 20.66%, with exact checksums, unchanged reductions/stub counts, zero
  deopts, and native-boundary code shrinking from 6,848 to 6,740 bytes;
- guarded global-object reads inside a completely generated outermost loop now
  cache the live property-slot address after one realm-epoch and shape proof.
  The loaded value remains live, and a cached builtin namespace becomes an
  activation-invariant receiver that unlocks its method-identity caches. Entry,
  OSR, and every generated miss clear global and method raw addresses together
  before collection or reentry. Artifact coverage exposes
  `loopInvariantGlobalObjectLoadCache`; a property getter replaces global
  `Math`, allocates under GC stress, and proves the next iteration observes both
  replacement methods. Alternating 10-warmup/25-sample medians improved the
  Math-only kernel by 47.29% and full `native-boundary` by 18.77%, with exact
  checksums, unchanged reductions/stub counts, zero deopts, and native-boundary
  code growing from 6,740 to 7,544 bytes;
- catch-only exception-region CFGs with explicit landing-pad successors; the
  shared linkage fully unwinds publication, commits the thrown value to its
  allocator home, and transfers at the exact call PC without replay;
- representation-checked entry and OSR guards with no replay after a started
  operation; method guard misses deopt before lookup effects and tier/frame
  publication changes do not require caller recompilation;
- one typed `RuntimeCall` boundary for scalar query/coercion, static value-load,
  class-construction, ordinary prototype mutation, and built-in Array iterator
  operations. These families
  decode machine operand words once into owned descriptors, operate on either
  materialized or generated stack-owned frames, and never recover an
  interpreter frame index merely to complete semantics.

The old optimizing compiler and template emitter remain in the active graph
only for operations and function shapes not yet selected by the replacement
pipeline. They are fallback, not contracts to preserve.

Latest accepted gate: 53 bytecode, 86 compiler, 283 JIT, and 843 VM tests;
the complete runtime suite; all-target/all-feature Clippy;
compile-fail/rooting checks; and 21/21 differential
interpreter/tier/GC-stress cases. Test262 was not run for the engine slices.

## Active work

### 1. Complete reentrant operation families

Plain calls, guarded methods, and fixed or spread base, derived, and superclass
constructs now use the same descriptor, allocator roots, stable entry cells,
frame publication, cold deopt machinery, and explicit catch landing edges.
Construction validates the generation before observable receiver work,
publishes or inherits `new.target`, preserves the derived-`this` TDZ, and
applies the one base/derived return contract without replaying `New` or
`SuperConstruct`. `CallSpread`, `NewSpread`, and `SuperConstructSpread` own
ordinary typed call feedback and bake monomorphic targets into this linkage.
The spread array remains rooted across cold resolution/receiver preparation
and a leaf runtime stub copies declared arguments directly into the unpublished
callee frame; no second call, frame, root, or result ABI exists. Generated
spread wrappers also keep the compiler-emitted default-Array iterator
collection native: `GetIterator`, `IteratorNext`, and `ArrayPush` operate over
the published stack-owned frame through the typed runtime boundary. An own,
replaced, accessor-backed, or non-Array iterator refuses before effects and
resumes the exact materialized path. Scalar query/coercion, static namespace /
BigInt/string-index loads, and class heritage / computed naming now use this
same boundary; their former `jit_runtime_*` materialized-frame entrypoints and
JIT-prefixed VM modules are deleted. Fixed base `new` consumes the same
generated linkage as plain and method calls, while fresh/inherited upvalues
remain stack-owned. Ordinary materialized data prototypes now allocate the base
receiver without reentry; the observable preparation sibling remains the
single authority for accessors, proxies, bound functions, and uncertain
  prototype state. Simple ordinary base initializers now share their one final
hidden-class contract with generated fixed, spread, and superclass receiver
allocation. The expanded causal kernel moved 4.2 million field additions out
of StoreProperty stubs, reduced total runtime-stub transitions by 54.44% and
reentrant transitions by 99.34%, and reduced the mean of alternating process
medians by 15.34%; reductions, bytecode calls, generated calls, and zero call
deopts remained identical. Extend the typed boundary to the remaining forms:

- admit nested catch/finally regions and complete multi-frame state through
  `lower_deopt_table`;
- move the remaining reentrant natives onto typed descriptors instead of
  opcode-specific materialization stubs;
- prove constructor abrupt completion and interrupt/budget exits without
  conservative scanning, replay, or an interpreter-window ABI.

Leaf calls remain the no-safepoint case of this same descriptor boundary; do
not add a second call format.

### 2. Move complete operation families onto Machine IR

Implement in this order:

1. nested/finally exceptional edges and multi-frame state;
2. complete general settled fields and elements with dependency tokens (the
   exact constructor-owned add-property slice is native);
3. inline object allocation, constructors, and virtual objects;
4. OSR at every reducible loop and incremental-GC handshakes.

Each slice must delete its old selector/emitter consumer from the active path.
Do not translate new IR back into legacy SSA or legacy allocation.

### 3. Complete target parity

- implement x86-64 selection, legalization, frame emission, calls, polls,
  safepoints, and deopt exits over the same Machine IR;
- keep JavaScript semantics above target selection;
- make normalized function-local artifacts insensitive to runtime addresses
  and unrelated source edits;
- require cross-target verifier and allocation tests for every shared opcode.

### 4. Perform the atomic compiler switch

Once one complete supported-language boundary exists on both targets:

- make quick and optimizing compilation choose budgets over the same pipeline;
- delete the template compiler, legacy optimizing SSA/regalloc/emitter, their
  interpreter-window ABI, duplicated direct emitters, and obsolete artifacts;
- remove caller-visible callee tier/frame assumptions that are no longer part
  of stable dispatch cells;
- leave the interpreter as the semantic oracle and tier fallback, not as a
  compiled-code ABI.

### 5. Shared semantic optimization

After the atomic switch, add representation propagation, dependency-aware
guard elimination, inlining, GVN, LICM, loop scheduling, and cold outlining.
Quick compilation skips expensive global passes; it does not use a different
backend or value/frame format.

## Required gates

Every substantial slice must prove the affected invariants with focused native
execution before the full gate. Before committing run:

```text
cargo fmt --all
cargo test -p otter-jit
cargo clippy -p otter-jit --all-targets --all-features -- -D warnings
bash scripts/gate.sh
```

Additional correctness gates for the final switch:

- recursive and mutually recursive calls;
- exceptions and reentrant natives;
- moving-GC stress across supported strides;
- nested inline deopt reconstruction;
- OSR entry/exit representation round trips;
- identical normalized artifacts under unrelated source edits;
- no conservative compiled-stack scan and no interpreter-window root ABI.

Performance claims require validated fresh-process A/B measurements. Retired
instructions are primary; wall time is a sanity check. Failed, unavailable, or
unvalidated observations remain visible and are never scoreable. The focused
kernel harness times actual tier-up compiler-hook invocations, snapshots final
native-code residency, and carries a derived-constructor workload that proves
hot fixed-arity `new Derived(...)` and `super(...)` linkage before a
construction performance claim is accepted.

## Stop conditions

Revise the architecture instead of adding a workaround if any occurs:

1. x86-64 needs a JavaScript-semantic lowering fork.
2. A moving root needs an interpreter window or conservative stack scan.
3. A caller must be recompiled when a callee changes tier or frame size.
4. Typed fields require another deopt, frame, or value schema.
5. Inline allocation cannot share ordinary FrameState and stack-map machinery.
6. Unrelated dead source changes normalized function-local artifacts.
7. Exact post-allocation root/deopt locations require pervasive forced spills.

## Working rules

- Parse JS/TS only through the existing AST frontend.
- Keep `otter-gc -> otter-vm -> otter-runtime -> product crates` dependency
  direction and never add `crates-legacy` to the active graph.
- Do not add compatibility readers, schema versions, adapters, replay paths,
  dual writers, or parallel IR/frame/value formats.
- Keep GC roots explicit and allocation-driven; native value building uses
  handle scopes.
- Keep `lib.rs` as a crate map and small glue surface.
- Use focused tests during development and update this plan only with current
  state, next work, and accepted gates.
