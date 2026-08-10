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
- catch-only exception-region CFGs with explicit landing-pad successors; the
  shared linkage fully unwinds publication, commits the thrown value to its
  allocator home, and transfers at the exact call PC without replay;
- representation-checked entry and OSR guards with no replay after a started
  operation; method guard misses deopt before lookup effects and tier/frame
  publication changes do not require caller recompilation.
- one typed `RuntimeCall` boundary for scalar query/coercion, static value-load,
  class-construction, ordinary prototype mutation, and built-in Array iterator
  operations. These families
  decode machine operand words once into owned descriptors, operate on either
  materialized or generated stack-owned frames, and never recover an
  interpreter frame index merely to complete semantics.

The old optimizing compiler and template emitter remain in the active graph
only for operations and function shapes not yet selected by the replacement
pipeline. They are fallback, not contracts to preserve.

Latest accepted gate: 53 bytecode, 86 compiler, 282 JIT, and 835 VM tests;
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
